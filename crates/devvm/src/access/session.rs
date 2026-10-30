//! The embedded SSH client: connect (with a pinned host key and the instance's client key), the keepalive, the VM-exit
//! watcher and the boot bound. `exec` is in `exec.rs`, SFTP in `sftp.rs`.
//!
//! Every blocking method is a facade: the work runs on the runtime of `runtime.rs`, and the calling thread waits on a
//! channel.
//!
//! **What ends a wait.** Nothing here is synchronised by a bare delay. A wait ends when its real condition holds or on
//! one of these: the VM process is gone ([`VmExit`]); the keepalive finds the peer dead; the **boot bound**, the failure
//! bound of a guest that is still booting; the **handshake bound**, the failure bound of one connection attempt to a
//! guest that is supposed to be up. Each failure names what was awaited and the serial log.

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use russh::client;
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, sleep_until, timeout};

use crate::access::runtime;
use crate::contain::VmProcess;
use crate::exit::{Output, Progress};
use crate::{Error, ErrorKind, Result};

pub(crate) use crate::access::exec::ExecEnd;
use crate::access::exec::{Event, run_exec};
use crate::access::sftp::Sftp;

#[cfg(test)]
#[path = "session_tests.rs"]
mod session_tests;

/// The pause between two connection attempts of a booting guest. It paces the loop; it never ends it.
const RETRY_PAUSE: Duration = Duration::from_millis(200);

/// Chunks of guest output the executing task may hold ahead of a slow host writer.
const EVENT_BUFFER: usize = 16;

// Parameters ==========================================================================================================

/// Where and as whom to connect, and what to name in errors.
#[derive(Clone, Debug)]
pub(crate) struct SshTarget {
    pub addr: std::net::SocketAddr,
    pub user: String,
    /// The OpenSSH text of the private key.
    pub client_private: String,
    /// The OpenSSH text of the host key the guest must present; any other is refused.
    pub host_public: String,
    /// Named in every VM-exit and boot-bound error.
    pub serial_log: PathBuf,
}

/// SSH keepalive: after `interval` of silence a probe is sent, and the connection is dead once more than `max_missed`
/// probes went unanswered.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Keepalive {
    pub interval: Duration,
    pub max_missed: u32,
}

impl Keepalive {
    /// 5 s and 3 misses (assumption of the plan).
    pub(crate) const STANDARD: Keepalive = Keepalive {
        interval: Duration::from_secs(5),
        max_missed: 3,
    };

    /// How long one connection attempt to a guest that should be up may take: the time the keepalive would need to
    /// declare a silent peer dead.
    pub(crate) fn handshake_bound(self) -> Duration {
        self.interval * self.max_missed
    }
}

/// Fires when the VM process is gone.
#[derive(Clone)]
pub(crate) struct VmExit {
    gone: Option<watch::Receiver<bool>>,
}

impl VmExit {
    /// Watches `process` from a thread that blocks in `VmProcess::wait`. The thread lives as long as the VM.
    pub(crate) fn watch(process: VmProcess) -> VmExit {
        VmExit::when(move || process.wait().is_ok())
    }

    /// Fires when `wait`, which blocks until the VM is gone, returns `true`. A `false` means the VM's end could not be
    /// observed, and the watcher then never fires.
    pub(crate) fn when(wait: impl FnOnce() -> bool + Send + 'static) -> VmExit {
        let (sender, receiver) = watch::channel(false);
        std::thread::Builder::new()
            .name("devvm-vm-exit".to_owned())
            .spawn(move || {
                if wait() {
                    let _ = sender.send(true);
                }
            })
            .expect("the OS can start a thread");
        VmExit { gone: Some(receiver) }
    }

    /// A watcher that never fires: for a machine whose process is not watched.
    pub(crate) fn never() -> VmExit {
        VmExit { gone: None }
    }

    /// Resolves once the VM is gone; never for [`VmExit::never`].
    pub(crate) async fn gone(&self) {
        let Some(receiver) = &self.gone else {
            return std::future::pending().await;
        };
        let mut receiver = receiver.clone();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                // The watcher ended without seeing the VM go.
                return std::future::pending().await;
            }
        }
    }
}

/// The failure bound on a booting guest: one deadline, started when the VM is resumed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BootBound {
    limit: Duration,
    deadline: Instant,
}

impl BootBound {
    pub(crate) fn start(limit: Duration) -> BootBound {
        BootBound {
            limit,
            deadline: Instant::now() + limit,
        }
    }
}

// Shared state ========================================================================================================

/// What the client handler of one connection tells the session.
struct Transport {
    /// The reason, once the connection has ended.
    ended: watch::Sender<Option<String>>,
    /// The fingerprint of a host key that was not the pinned one.
    refused_key: Mutex<Option<String>>,
}

impl Transport {
    fn new() -> Arc<Transport> {
        Arc::new(Transport {
            ended: watch::channel(None).0,
            refused_key: Mutex::new(None),
        })
    }

    /// Resolves with the reason once the connection has ended.
    async fn ended(&self) -> String {
        let mut receiver = self.ended.subscribe();
        loop {
            if let Some(reason) = receiver.borrow_and_update().clone() {
                return reason;
            }
            if receiver.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }
}

pub(super) struct ClientHandler {
    pinned: PublicKey,
    transport: Arc<Transport>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> std::result::Result<bool, Self::Error> {
        let presented = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => PublicKey::from(cert.public_key().clone()),
        };
        if presented.key_data() == self.pinned.key_data() {
            return Ok(true);
        }
        *self.transport.refused_key.lock().unwrap() = Some(presented.fingerprint(HashAlg::Sha256).to_string());
        Ok(false)
    }

    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> std::result::Result<(), Self::Error> {
        let text = match &reason {
            client::DisconnectReason::ReceivedDisconnect(info) => {
                format!("the server closed the connection ({info:?})")
            }
            client::DisconnectReason::Error(russh::Error::KeepaliveTimeout) => {
                "the server stopped answering keepalives".to_owned()
            }
            client::DisconnectReason::Error(e) => format!("the connection failed: {e}"),
        };
        self.transport.ended.send_replace(Some(text));
        match reason {
            client::DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            client::DisconnectReason::Error(e) => Err(e),
        }
    }
}

// Errors ==============================================================================================================

/// What is known about a session for its errors.
#[derive(Clone)]
pub(super) struct Context {
    pub addr: std::net::SocketAddr,
    pub user: String,
    pub serial_log: PathBuf,
    pub host_public: String,
}

impl Context {
    pub(super) fn lost(&self, awaiting: &str, why: &str) -> Error {
        Error::new(
            ErrorKind::ConnectionLost,
            format!("{awaiting}: {why} (serial log: {})", self.serial_log.display()),
        )
    }

    fn vm_exited(&self, awaiting: &str) -> Error {
        self.lost(awaiting, "the VM process exited")
    }

    fn bound_passed(&self, awaiting: &str, limit: Duration) -> Error {
        self.lost(
            awaiting,
            &format!("it did not happen within the boot bound of {limit:?}"),
        )
    }
}

/// Why one connection attempt failed.
enum Failure {
    Tcp(std::io::Error),
    Transport(russh::Error),
    HostKey { presented: String },
    Rejected { remaining: String },
}

impl Failure {
    fn into_error(self, ctx: &Context, bound: Option<Duration>) -> Error {
        let awaiting = format!("waiting for SSH on {} to answer", ctx.addr);
        let within = bound.map_or(String::new(), |b| format!(" within {b:?}"));
        match self {
            Failure::Tcp(e) => ctx.lost(&awaiting, &format!("no answer{within}; the connection failed: {e}")),
            Failure::Transport(e) => ctx.lost(&awaiting, &format!("no answer{within}; the handshake failed: {e}")),
            Failure::HostKey { presented } => Error::new(
                ErrorKind::HostKey,
                format!(
                    "SSH on {} presented the host key {presented}, not the pinned key {}",
                    ctx.addr,
                    pinned_fingerprint(&ctx.host_public)
                ),
            ),
            Failure::Rejected { remaining } => Error::new(
                ErrorKind::Io,
                format!(
                    "SSH on {} refused the client key of user `{}` (the server still offers: {remaining})",
                    ctx.addr, ctx.user
                ),
            ),
        }
    }
}

fn pinned_fingerprint(host_public: &str) -> String {
    PublicKey::from_openssh(host_public)
        .map(|key| key.fingerprint(HashAlg::Sha256).to_string())
        .unwrap_or_else(|_| "(unreadable)".to_owned())
}

// The session =========================================================================================================

pub(super) struct Inner {
    pub(super) handle: client::Handle<ClientHandler>,
    pub(super) ctx: Context,
    transport: Arc<Transport>,
    vm: VmExit,
    boot: Mutex<Option<BootBound>>,
}

/// A connection to a guest's SSH server.
#[derive(Clone)]
pub(crate) struct SshSession {
    pub(super) inner: Arc<Inner>,
}

async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

impl Inner {
    fn boot_bound(&self) -> Option<BootBound> {
        *self.boot.lock().unwrap()
    }

    /// Runs `operation`, ending it early when the VM process exits, when a booting guest passes its boot bound and,
    /// if `watch_transport`, when the connection ends. `awaiting` names the operation in those errors.
    ///
    /// An `operation` that sees the end of the connection itself (a channel that closes) passes `false`: this race
    /// could otherwise report a lost connection for a command that has finished.
    pub(super) async fn guarded<T>(
        &self,
        awaiting: &str,
        watch_transport: bool,
        operation: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let bound = self.boot_bound();
        let transport_ended = async {
            if watch_transport {
                self.transport.ended().await
            } else {
                std::future::pending().await
            }
        };
        tokio::select! {
            biased;
            result = operation => result,
            () = self.vm.gone() => Err(self.ctx.vm_exited(awaiting)),
            () = until(bound.map(|b| b.deadline)) => {
                Err(self.ctx.bound_passed(awaiting, bound.map_or(Duration::ZERO, |b| b.limit)))
            }
            reason = transport_ended => Err(self.ctx.lost(awaiting, &reason)),
        }
    }

    /// The error for a connection that ended: the reason the client handler recorded, if it did.
    pub(super) fn connection_ended(&self, awaiting: &str) -> Error {
        let reason = self
            .transport
            .ended
            .borrow()
            .clone()
            .unwrap_or_else(|| "the connection ended".to_owned());
        self.ctx.lost(awaiting, &reason)
    }
}

/// Everything one connect needs, owned so it can move into the runtime.
struct Connect {
    target: SshTarget,
    pinned: PublicKey,
    key: Arc<PrivateKey>,
    ka: Keepalive,
    vm: VmExit,
    boot: Option<BootBound>,
    ctx: Context,
}

impl Connect {
    fn config(&self) -> Arc<client::Config> {
        Arc::new(client::Config {
            keepalive_interval: Some(self.ka.interval),
            keepalive_max: self.ka.max_missed as usize,
            nodelay: true,
            ..client::Config::default()
        })
    }

    /// One attempt: TCP, banner and key exchange (the host key is checked there), authentication.
    async fn attempt(
        &self,
        config: &Arc<client::Config>,
    ) -> std::result::Result<(client::Handle<ClientHandler>, Arc<Transport>), Failure> {
        let stream = TcpStream::connect(self.target.addr).await.map_err(Failure::Tcp)?;
        let transport = Transport::new();
        let handler = ClientHandler {
            pinned: self.pinned.clone(),
            transport: transport.clone(),
        };
        let mut handle = match client::connect_stream(config.clone(), stream, handler).await {
            Ok(handle) => handle,
            Err(e) => {
                return Err(match transport.refused_key.lock().unwrap().take() {
                    Some(presented) => Failure::HostKey { presented },
                    None => Failure::Transport(e),
                });
            }
        };
        let auth = handle
            .authenticate_publickey(&self.target.user, PrivateKeyWithHashAlg::new(self.key.clone(), None))
            .await
            .map_err(Failure::Transport)?;
        match auth {
            client::AuthResult::Success => Ok((handle, transport)),
            client::AuthResult::Failure { remaining_methods, .. } => Err(Failure::Rejected {
                remaining: format!("{remaining_methods:?}"),
            }),
        }
    }

    fn session(self, parts: (client::Handle<ClientHandler>, Arc<Transport>)) -> SshSession {
        let (handle, transport) = parts;
        SshSession {
            inner: Arc::new(Inner {
                handle,
                ctx: self.ctx,
                transport,
                vm: self.vm,
                boot: Mutex::new(self.boot),
            }),
        }
    }

    /// A single attempt that is final, bounded by the handshake bound and the VM's end.
    async fn once(self) -> Result<SshSession> {
        let awaiting = format!("waiting for SSH on {} to answer", self.ctx.addr);
        let config = self.config();
        let bound = self.ka.handshake_bound();
        let outcome = tokio::select! {
            outcome = timeout(bound, self.attempt(&config)) => outcome,
            () = self.vm.gone() => return Err(self.ctx.vm_exited(&awaiting)),
        };
        match outcome {
            Ok(Ok(parts)) => Ok(self.session(parts)),
            Ok(Err(failure)) => Err(failure.into_error(&self.ctx, None)),
            Err(_) => Err(self
                .ctx
                .lost(&awaiting, &format!("no answer within the handshake bound of {bound:?}"))),
        }
    }

    /// Attempts until one succeeds, the VM exits or the boot bound passes.
    async fn until_up(self, bound: BootBound, messages: mpsc::UnboundedSender<String>) -> Result<SshSession> {
        let awaiting = format!("waiting for SSH on {} to answer", self.ctx.addr);
        let config = self.config();
        let mut last: Option<Failure> = None;
        loop {
            tokio::select! {
                outcome = self.attempt(&config) => match outcome {
                    Ok(parts) => return Ok(self.session(parts)),
                    Err(failure) => {
                        if last.is_none() {
                            let _ = messages.send(format!("waiting for SSH on {}", self.ctx.addr));
                        }
                        last = Some(failure);
                    }
                },
                () = self.vm.gone() => return Err(self.ctx.vm_exited(&awaiting)),
                () = sleep_until(bound.deadline.into()) => return Err(self.out_of_time(last, &awaiting, bound)),
            }
            tokio::select! {
                () = sleep(RETRY_PAUSE) => {}
                () = self.vm.gone() => return Err(self.ctx.vm_exited(&awaiting)),
                () = sleep_until(bound.deadline.into()) => return Err(self.out_of_time(last, &awaiting, bound)),
            }
        }
    }

    fn out_of_time(&self, last: Option<Failure>, awaiting: &str, bound: BootBound) -> Error {
        match last {
            Some(failure) => failure.into_error(&self.ctx, Some(bound.limit)),
            None => self.ctx.bound_passed(awaiting, bound.limit),
        }
    }
}

fn key_error(what: &str, source: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::new(ErrorKind::Internal, format!("{what}: {source}")).with_source(source)
}

impl SshSession {
    /// Connects to the guest and authenticates. `boot` is `Some` for a guest that is still booting: the connect then
    /// retries refusals, resets, a host key that does not match yet and a client key that is not accepted yet (cloud-init
    /// publishes the host key and the authorized key in separate steps) until it succeeds, the VM exits or the boot bound
    /// passes, and the session stays "booting" until [`SshSession::finish_boot`].
    ///
    /// With `boot: None` it makes exactly one attempt and every failure is final: a running guest's keys are in place,
    /// so a mismatch there is a changed key. The attempt is bounded by [`Keepalive::handshake_bound`], so a peer that
    /// accepts the connection and then says nothing (a stopped VM, a hung `sshd`) ends as `connection-lost`.
    ///
    /// Failures: `connection-lost`, `host-key` (a mismatch, naming both fingerprints) and `io` (a rejected client key).
    pub(crate) fn connect(
        target: &SshTarget,
        ka: Keepalive,
        vm: VmExit,
        boot: Option<BootBound>,
        progress: &dyn Progress,
    ) -> Result<SshSession> {
        let pinned = PublicKey::from_openssh(&target.host_public)
            .map_err(|e| key_error("the pinned host key is not an OpenSSH public key", e))?;
        let key = PrivateKey::from_openssh(&target.client_private)
            .map_err(|e| key_error("the client key is not an OpenSSH private key", e))?;
        let connect = Connect {
            ctx: Context {
                addr: target.addr,
                user: target.user.clone(),
                serial_log: target.serial_log.clone(),
                host_public: target.host_public.clone(),
            },
            target: target.clone(),
            pinned,
            key: Arc::new(key),
            ka,
            vm,
            boot,
        };
        runtime::run_reporting("connecting over SSH", progress, move |messages| async move {
            match connect.boot {
                Some(bound) => connect.until_up(bound, messages).await,
                None => connect.once().await,
            }
        })?
    }

    /// Ends the boot phase: from now on the boot bound no longer ends a wait.
    pub(crate) fn finish_boot(&self) {
        *self.inner.boot.lock().unwrap() = None;
    }

    /// Runs `command` through the guest's SSH `exec`, copying guest stdout and stderr to `out` and `err` as they
    /// arrive, and flushing both when the command ends. A guest that outpaces the writers is slowed down, not buffered. See [`ExecEnd`] for the two ways it
    /// ends. Environment is never sent as SSH `env` requests: the command string carries it.
    ///
    /// Failures: `connection-lost` (keepalive expiry, a dropped connection, the VM exiting, the boot bound),
    /// `no-exit-status` (the channel closed without a status while the connection lives), `io`.
    pub(crate) fn exec(
        &self,
        command: &str,
        out: &mut dyn std::io::Write,
        err: &mut dyn std::io::Write,
    ) -> Result<ExecEnd> {
        let (sender, mut receiver) = mpsc::channel(EVENT_BUFFER);
        let inner = self.inner.clone();
        let command = command.to_owned();
        runtime::spawn(run_exec(inner, command, sender));
        loop {
            let (to_stderr, chunk) = match runtime::wait(receiver.recv()) {
                Some(Event::Out(chunk)) => (false, chunk),
                Some(Event::Err(chunk)) => (true, chunk),
                Some(Event::End(result)) => {
                    // A partial line the host writers still buffer (a prompt) would otherwise be lost.
                    let flushed = out.flush().and_then(|()| err.flush());
                    return match (result, flushed) {
                        (Ok(status), Ok(())) => Ok(ExecEnd::Finished(status)),
                        (Ok(_), Err(e)) => Ok(ExecEnd::HostOutputClosed(e)),
                        (Err(e), _) => Err(e),
                    };
                }
                None => {
                    return Err(Error::new(
                        ErrorKind::Internal,
                        "the SSH task for the command ended without a result",
                    ));
                }
            };
            let sink: &mut dyn std::io::Write = if to_stderr { &mut *err } else { &mut *out };
            if let Err(e) = sink.write_all(&chunk) {
                // Dropping the receiver tells the task to close the channel.
                drop(receiver);
                return Ok(ExecEnd::HostOutputClosed(e));
            }
        }
    }

    /// [`SshSession::exec`] into memory: the sinks are `Vec`s, so the host output never closes.
    pub(crate) fn exec_collect(&self, command: &str) -> Result<Output> {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        match self.exec(command, &mut stdout, &mut stderr)? {
            ExecEnd::Finished(status) => Ok(Output { status, stdout, stderr }),
            ExecEnd::HostOutputClosed(e) => Err(Error::io(ErrorKind::Internal, "a memory sink refused output", e)),
        }
    }

    /// Opens the SFTP subsystem.
    pub(crate) fn sftp(&self) -> Result<Sftp> {
        Sftp::open(self.inner.clone())
    }
}
