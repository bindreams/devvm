//! The embedded SSH client: connect (with a pinned host key and the instance's client key), the keepalive, the VM-exit
//! watcher and the boot bound. `exec` is in `exec.rs`, SFTP in `sftp.rs`.
//!
//! Every blocking method is a facade: the work runs on the runtime of `runtime.rs`, and the calling thread waits on a
//! channel.
//!
//! **What ends a wait.** Nothing here is synchronised by a bare delay. A wait ends when its real condition holds or on
//! one of these: the VM process is gone ([`VmExit`]); the keepalive finds the peer dead; the **boot bound**, the
//! failure bound of a guest that is still booting; the **handshake bound**, the failure bound of one connection attempt
//! to a guest that is supposed to be up. Each failure names what was awaited and the serial log.

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use russh::client;
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use secrecy::{ExposeSecret, SecretString};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, sleep_until, timeout};

use self::watched::Watched;
use crate::access::runtime;
use crate::contain::VmProcess;
use crate::exit::{Output, Progress};
use crate::{Error, ErrorKind, Result};

pub(crate) use crate::access::exec::ExecEnd;
use crate::access::exec::{Event, run_exec};
use crate::access::sftp::Sftp;

pub(super) mod watched;

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
    pub client_private: SecretString,
    /// The OpenSSH text of the host key the guest must present; any other is refused.
    pub host_public: String,
    /// Named in every VM-exit and boot-bound error.
    pub serial_log: PathBuf,
}

/// SSH keepalive: after `interval` of silence a probe is sent, and the connection is dead once more than `max_missed`
/// probes went unanswered. [`SshSession::connect`] refuses a zero `interval` or `max_missed`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Keepalive {
    pub interval: Duration,
    pub max_missed: u32,
}

impl Keepalive {
    /// 5 s and 3 misses.
    pub(crate) const STANDARD: Keepalive = Keepalive {
        interval: Duration::from_secs(5),
        max_missed: 3,
    };

    /// How long one connection attempt to a guest that should be up may take: the time the keepalive would need to
    /// declare a silent peer dead, which is `max_missed + 1` silent intervals.
    ///
    /// Refuses (kind `internal`, naming the field) what would never fire or cannot be represented: an `interval` of 0
    /// (every probe would be missed at once), a `max_missed` of 0 (SSH reads it as "never give up on a silent peer"),
    /// and a pair whose product does not fit in a `Duration`.
    pub(crate) fn handshake_bound(self) -> Result<Duration> {
        if self.interval.is_zero() {
            return Err(Error::new(
                ErrorKind::Internal,
                "a keepalive with an interval of 0 would miss every probe at once; interval must be positive",
            ));
        }
        if self.max_missed == 0 {
            return Err(Error::new(
                ErrorKind::Internal,
                "a keepalive that tolerates no missed probes would never fire; max_missed must be at least 1",
            ));
        }
        self.max_missed
            .checked_add(1)
            .and_then(|silent_intervals| self.interval.checked_mul(silent_intervals))
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Internal,
                    format!("a keepalive of {self:?} has a handshake bound too long to represent; lower max_missed or interval"),
                )
            })
    }
}

/// How the VM watcher ended.
#[derive(Clone, Debug)]
pub(crate) enum WatchEnd {
    /// The VM process is gone.
    Exited,
    /// The watcher could not tell: the reason it gives. A wait that cannot know whether the VM lives must not go on
    /// as if it did.
    Unwatched(String),
}

/// Fires when the VM process is gone, or when it can no longer be watched.
#[derive(Clone)]
pub(crate) struct VmExit {
    ended: Option<watch::Receiver<Option<WatchEnd>>>,
}

impl VmExit {
    /// Watches `process` from a thread that blocks in `VmProcess::wait`. The thread lives as long as the VM.
    pub(crate) fn watch(process: VmProcess) -> Result<VmExit> {
        VmExit::when(move || process.wait().map_err(|e| e.message().to_owned()))
    }

    /// Fires when `wait`, which blocks until the VM is gone, returns. An `Err` is the reason the VM's end could not be
    /// observed; waits then end with it. Fails when the OS cannot start the thread.
    pub(crate) fn when(wait: impl FnOnce() -> std::result::Result<(), String> + Send + 'static) -> Result<VmExit> {
        let (sender, receiver) = watch::channel(None);
        std::thread::Builder::new()
            .name("devvm-vm-exit".to_owned())
            .spawn(move || {
                let end = match wait() {
                    Ok(()) => WatchEnd::Exited,
                    Err(why) => WatchEnd::Unwatched(why),
                };
                let _ = sender.send(Some(end));
            })
            .map_err(|e| Error::io(ErrorKind::Io, "cannot start the thread that watches the VM process", e))?;
        Ok(VmExit { ended: Some(receiver) })
    }

    /// A watcher that never fires: for a machine whose process is not watched.
    pub(crate) fn never() -> VmExit {
        VmExit { ended: None }
    }

    /// Resolves once the VM is gone or unwatchable; never for [`VmExit::never`].
    pub(crate) async fn gone(&self) -> WatchEnd {
        let Some(receiver) = &self.ended else {
            return std::future::pending().await;
        };
        let mut receiver = receiver.clone();
        loop {
            if let Some(end) = receiver.borrow_and_update().clone() {
                return end;
            }
            if receiver.changed().await.is_err() {
                // The watcher thread died without a verdict (it panicked).
                return WatchEnd::Unwatched("the thread that watches the VM process ended without a result".to_owned());
            }
        }
    }
}

/// `duration` in units a person reads (`1h`, `1m 30s`).
pub(crate) fn humanly(duration: Duration) -> String {
    jiff::SignedDuration::try_from(duration).map_or_else(|_| format!("{duration:?}"), |signed| format!("{signed:#}"))
}

/// The failure bound on a booting guest: one deadline, started when the VM is resumed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BootBound {
    limit: Duration,
    deadline: Instant,
}

impl BootBound {
    /// Starts the clock. Refuses (kind `internal`) a `limit` too long for the clock to represent.
    pub(crate) fn start(limit: Duration) -> Result<BootBound> {
        let deadline = Instant::now().checked_add(limit).ok_or_else(|| {
            Error::new(
                ErrorKind::Internal,
                format!("a boot bound of {} is too long to represent; lower it", humanly(limit)),
            )
        })?;
        Ok(BootBound { limit, deadline })
    }
}

/// The boot bound of a session while the guest is booting. Waits in flight follow changes to it.
struct BootGate(watch::Sender<Option<BootBound>>);

impl BootGate {
    fn new(bound: Option<BootBound>) -> BootGate {
        BootGate(watch::channel(bound).0)
    }

    /// From now on no wait is ended by the bound, the ones in flight included.
    fn finish(&self) {
        self.0.send_replace(None);
    }

    /// Resolves with the bound once its deadline passes; never while there is none.
    async fn passed(&self) -> BootBound {
        let mut receiver = self.0.subscribe();
        loop {
            let bound = *receiver.borrow_and_update();
            match bound {
                Some(bound) => {
                    tokio::select! {
                        () = sleep_until(bound.deadline.into()) => return bound,
                        _ = receiver.changed() => {}
                    }
                }
                None => {
                    // The sender lives in `self`, so this does not fail while `self` is borrowed.
                    let _ = receiver.changed().await;
                }
            }
        }
    }
}

// Shared state ========================================================================================================

/// What the client handler of one connection tells the session. A stream that is watched for its end ([`Watched`])
/// records into one too: the SFTP channels, whose end is not the connection's.
pub(in crate::access) struct Transport {
    /// The reason, once the connection has ended.
    ended: watch::Sender<Option<String>>,
    /// The fingerprint of a host key that was not the pinned one.
    refused_key: Mutex<Option<String>>,
}

impl Transport {
    pub(in crate::access) fn new() -> Arc<Transport> {
        Arc::new(Transport {
            ended: watch::channel(None).0,
            refused_key: Mutex::new(None),
        })
    }

    /// Records the end of the connection, unless an earlier report did: the first reason stays.
    pub(in crate::access) fn end(&self, reason: String) {
        self.ended.send_if_modified(|ended| {
            let first = ended.is_none();
            if first {
                *ended = Some(reason);
            }
            first
        });
    }

    /// Resolves with the reason once the connection has ended.
    pub(in crate::access) async fn ended(&self) -> String {
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
        self.transport.end(text);
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

    fn vm_ended(&self, awaiting: &str, end: &WatchEnd) -> Error {
        match end {
            WatchEnd::Exited => self.lost(awaiting, "the VM process exited"),
            WatchEnd::Unwatched(why) => self.lost(
                awaiting,
                &format!("it cannot be told whether the VM process is still running ({why})"),
            ),
        }
    }

    fn bound_passed(&self, awaiting: &str, limit: Duration) -> Error {
        self.lost(
            awaiting,
            &format!("it did not happen within the boot bound of {}", humanly(limit)),
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
        let within = bound.map_or(String::new(), |b| format!(" within {}", humanly(b)));
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
    boot: BootGate,
}

/// A connection to a guest's SSH server.
#[derive(Clone)]
pub(crate) struct SshSession {
    pub(super) inner: Arc<Inner>,
}

impl Inner {
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
            end = self.vm.gone() => Err(self.ctx.vm_ended(awaiting, &end)),
            bound = self.boot.passed() => Err(self.ctx.bound_passed(awaiting, bound.limit)),
            reason = transport_ended => Err(self.ctx.lost(awaiting, &reason)),
        }
    }

    /// The reason the connection ended, once the client handler or the stream has recorded one.
    pub(super) fn transport_end(&self) -> Option<String> {
        self.transport.ended.borrow().clone()
    }

    /// The error for a connection that ended: the reason recorded, if there is one.
    pub(super) fn connection_ended(&self, awaiting: &str) -> Error {
        let reason = self
            .transport_end()
            .unwrap_or_else(|| "the connection ended".to_owned());
        self.ctx.lost(awaiting, &reason)
    }
}

/// The TCP connection to the guest's forwarded port. `russh` sets TCP_NODELAY only in the `client::connect` that makes
/// its own connection, and this one is made here, so it is set here: small requests (SFTP's stat, mkdir, rename) must
/// not wait for Nagle's algorithm.
async fn connect_tcp(addr: std::net::SocketAddr) -> std::io::Result<TcpStream> {
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// Everything one connect needs, owned so it can move into the runtime.
struct Connect {
    target: SshTarget,
    pinned: PublicKey,
    key: Arc<PrivateKey>,
    ka: Keepalive,
    /// [`Keepalive::handshake_bound`] of `ka`, which `SshSession::connect` has checked.
    handshake_bound: Duration,
    vm: VmExit,
    boot: Option<BootBound>,
    ctx: Context,
}

impl Connect {
    fn config(&self) -> Arc<client::Config> {
        Arc::new(client::Config {
            keepalive_interval: Some(self.ka.interval),
            keepalive_max: self.ka.max_missed as usize,
            ..client::Config::default()
        })
    }

    /// One attempt: TCP, banner and key exchange (the host key is checked there), authentication.
    async fn attempt(
        &self,
        config: &Arc<client::Config>,
    ) -> std::result::Result<(client::Handle<ClientHandler>, Arc<Transport>), Failure> {
        let stream = connect_tcp(self.target.addr).await.map_err(Failure::Tcp)?;
        let transport = Transport::new();
        let stream = Watched::new(stream, transport.clone());
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
                boot: BootGate::new(self.boot),
            }),
        }
    }

    /// A single attempt that is final, bounded by the handshake bound and the VM's end.
    async fn once(self) -> Result<SshSession> {
        let awaiting = format!("waiting for SSH on {} to answer", self.ctx.addr);
        let config = self.config();
        let bound = self.handshake_bound;
        let outcome = tokio::select! {
            outcome = timeout(bound, self.attempt(&config)) => outcome,
            end = self.vm.gone() => return Err(self.ctx.vm_ended(&awaiting, &end)),
        };
        match outcome {
            Ok(Ok(parts)) => Ok(self.session(parts)),
            Ok(Err(failure)) => Err(failure.into_error(&self.ctx, None)),
            Err(_) => Err(self.ctx.lost(
                &awaiting,
                &format!("no answer within the handshake bound of {}", humanly(bound)),
            )),
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
                end = self.vm.gone() => return Err(self.ctx.vm_ended(&awaiting, &end)),
                () = sleep_until(bound.deadline.into()) => return Err(self.out_of_time(last, &awaiting, bound)),
            }
            tokio::select! {
                () = sleep(RETRY_PAUSE) => {}
                end = self.vm.gone() => return Err(self.ctx.vm_ended(&awaiting, &end)),
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
    /// retries refusals, resets, a host key that does not match yet and a client key that is not accepted yet
    /// (cloud-init publishes the host key and the authorized key in separate steps) until it succeeds, the VM exits or
    /// the boot bound passes, and the session stays "booting" until [`SshSession::finish_boot`].
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
        let handshake_bound = ka.handshake_bound()?;
        let pinned = PublicKey::from_openssh(&target.host_public)
            .map_err(|e| key_error("the pinned host key is not an OpenSSH public key", e))?;
        let key = PrivateKey::from_openssh(target.client_private.expose_secret())
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
            handshake_bound,
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

    /// Ends the boot phase: from now on the boot bound no longer ends a wait, one already in flight included.
    pub(crate) fn finish_boot(&self) {
        self.inner.boot.finish();
    }

    /// Runs `command` through the guest's SSH `exec`, copying guest stdout and stderr to `out` and `err` as they
    /// arrive, and flushing both when the command ends. A guest that outpaces the writers is slowed down, not buffered.
    /// See [`ExecEnd`] for the three ways it ends. Environment is never sent as SSH `env` requests: the command string
    /// carries it.
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
        let mut lost = None;
        let inner = self.inner.clone();
        let command = command.to_owned();
        runtime::spawn(run_exec(inner, command, sender))?;
        loop {
            let (to_stderr, chunk) = match runtime::wait(receiver.recv()) {
                Some(Event::Out(chunk)) => (false, chunk),
                Some(Event::Err(chunk)) => (true, chunk),
                Some(Event::End(result)) => {
                    // A partial line the host writers still buffer (a prompt) would otherwise be lost.
                    let flushed_out = out.flush();
                    let flushed = flushed_out.and(err.flush());
                    return match (result, flushed, lost) {
                        (Ok(status), Ok(()), None) => Ok(ExecEnd::Finished(status)),
                        (Ok(status), Ok(()), Some(lost)) => Ok(ExecEnd::FinishedWithLoss { status, lost }),
                        (Ok(status), Err(error), lost) => Ok(ExecEnd::HostOutputClosed {
                            error: match lost {
                                None => error,
                                Some(lost) => {
                                    let text = format!("{error}; the command's output was lost as well ({lost})");
                                    std::io::Error::new(error.kind(), text)
                                }
                            },
                            status: Some(status),
                        }),
                        (Err(e), Err(error), _) => {
                            Err(e.together_with(format!("flushing the host output failed ({error})")))
                        }
                        (Err(e), Ok(()), _) => Err(e),
                    };
                }
                Some(Event::Lost(error)) => {
                    lost.get_or_insert(error);
                    continue;
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
                // The other sink may hold a partial line (a prompt); its flush is best effort, the write failure is
                // the result.
                let other: &mut dyn std::io::Write = if to_stderr { &mut *out } else { &mut *err };
                let _ = other.flush();
                // Dropping the receiver tells the task to close the channel.
                drop(receiver);
                return Ok(ExecEnd::HostOutputClosed { error: e, status: None });
            }
        }
    }

    /// [`SshSession::exec`] into memory: the sinks are `Vec`s, so the host output never closes. What is lost after the
    /// status is in [`Output::loss`].
    pub(crate) fn exec_collect(&self, command: &str) -> Result<Output> {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        match self.exec(command, &mut stdout, &mut stderr)? {
            ExecEnd::Finished(status) => Ok(Output {
                status,
                stdout,
                stderr,
                loss: None,
            }),
            ExecEnd::FinishedWithLoss { status, lost } => Ok(Output {
                status,
                stdout,
                stderr,
                loss: Some(lost),
            }),
            ExecEnd::HostOutputClosed { error, .. } => {
                Err(Error::io(ErrorKind::Internal, "a memory sink refused output", error))
            }
        }
    }

    /// Opens the SFTP subsystem.
    pub(crate) fn sftp(&self) -> Result<Sftp> {
        Sftp::open(self.inner.clone())
    }
}
