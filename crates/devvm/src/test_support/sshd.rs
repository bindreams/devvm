//! A scripted SSH and SFTP server on `127.0.0.1:0`, for the tests of the access layer.
//!
//! The command a client runs selects a script:
//!
//! | command | what the server does |
//! |---|---|
//! | `exit N` | exits with status N |
//! | `say-error-and-exit-125` | prints `devvm-error: x: y` on stdout, exits 125 |
//! | `signal NAME [core]` | ends with `exit-signal NAME` |
//! | `close-no-status` | closes the channel with no status |
//! | `drop-connection` | writes a line, then drops the TCP connection |
//! | `freeze` | stops reading and writing altogether (a frozen peer) |
//! | `pattern MiB` | writes MiB MiB of the 256-byte-value pattern to stdout, the same reversed to stderr, then exits 0 |
//! | `stdout-mib MiB` | writes MiB MiB of the pattern to stdout, then exits 0 |
//! | `write-forever` | writes to stdout until the client closes the channel |
//! | `hold` | does nothing and never answers: no output, no status |
//! | `write-once-and-hold` | writes one line to stdout, then does nothing and never answers |
//! | `status-then-marker` | sends `exit-status 7`, then the line `marker`, and never closes the channel |
//! | `extended-data N` | writes a line to extended-data stream N (1 is stderr), then exits 0 |
//! | `extended-data-and-hold N` | writes a line to extended-data stream N, then does nothing and never answers |
//! | `status-then-extended-data N` | writes `before the status` to stdout, sends `exit-status 7`, then a line to stream N, closes |
//! | `echo ...` | prints the rest of the line, exits 0 |
//! | `read-stdin` | waits for the client to send EOF, then prints `stdin ended` and exits 0 |
//!
//! Options make connections misbehave: a host key other than the advertised one, a refused client key, a peer that
//! accepts the connection and never speaks. The SFTP part serves a directory, with virtual symlinks, modes,
//! read-only directories and data failures that tests declare, and files that are generated or discarded so a multi-GiB
//! copy needs no disk.

mod fs;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{Auth, Config, Handler, Msg, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, Sig};
use russh_sftp::protocol::StatusCode;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::Error;
use crate::access::keys::InstanceKeys;
use crate::access::runtime;
use crate::access::{BootBound, Keepalive, SshSession, SshTarget, VmExit};
use crate::exit::NullProgress;

pub use fs::{BIG_SIZE, big_file_digest, expected_big_digest, pattern_byte, sample_windows};

/// Where Debian and Ubuntu put OpenSSH's SFTP server (package `openssh-sftp-server`).
pub const OPENSSH_SFTP_SERVER: &str = "/usr/lib/openssh/sftp-server";

/// How the server misbehaves.
#[derive(Clone, Default)]
pub struct SshdOptions {
    /// The OpenSSH text of the one client key that may log in.
    pub client_public: String,
    /// The first this-many connections present a host key other than [`TestSshd::host_public`].
    pub wrong_host_key_connections: usize,
    /// The first this-many connections refuse the client key.
    pub rejected_key_connections: usize,
    /// Accept the TCP connection and say nothing.
    pub silent: bool,
    /// The directory the SFTP subsystem serves; `None` serves a fresh temporary one.
    pub root: Option<PathBuf>,
    /// Drop the connection once this many bytes were read or written over SFTP.
    pub kill_sftp_after: Option<u64>,
    /// Refuse every `symlink` request, as a Windows guest does.
    pub no_symlinks: bool,
    /// Close the SFTP channel, and only it, once this many bytes were read or written over SFTP: the connection lives.
    pub close_sftp_after: Option<u64>,
    /// An SFTP `realpath` is never answered: a request in flight on the server, for tests that end the connection
    /// under it.
    pub hold_realpath: bool,
    /// Say that nothing exists, not even `/`, and refuse every `mkdir`: a guest whose file system cannot be asked.
    pub hollow: bool,
    /// What `realpath .` answers, the guest user's home; `None` answers `/`.
    pub home: Option<String>,
    /// Serve SFTP with OpenSSH's own `sftp-server` ([`OPENSSH_SFTP_SERVER`]), started in [`SshdOptions::root`], instead
    /// of the scripted one: the options and methods that script the SFTP part do not apply.
    pub openssh_sftp: bool,
    /// Show no modification time of any file, as a server that does not keep them.
    pub no_times: bool,
    /// Answer a directory listing this many entries at a time; `None` answers it in one reply.
    pub readdir_batch: Option<usize>,
    /// Refuse `posix-rename@openssh.com`, as a server that is not OpenSSH does.
    pub no_posix_rename: bool,
}

pub(crate) struct State {
    good: PrivateKey,
    bad: PrivateKey,
    allowed: PublicKey,
    pub(crate) options: SshdOptions,
    connections: AtomicUsize,
    closed: Mutex<HashSet<ChannelId>>,
    closed_notify: Notify,
    /// How many `exec` commands the server has received.
    started: AtomicUsize,
    started_notify: Notify,
    // The SFTP part.
    pub(crate) root: PathBuf,
    pub(crate) links: Mutex<HashMap<String, String>>,
    pub(crate) modes: Mutex<HashMap<String, u32>>,
    pub(crate) read_only_dirs: Mutex<HashSet<String>>,
    pub(crate) failing_data: Mutex<HashSet<String>>,
    pub(crate) phantoms: Mutex<HashSet<String>>,
    pub(crate) unlookable: Mutex<HashMap<String, StatusCode>>,
    pub(crate) listed_again: Mutex<HashSet<String>>,
    pub(crate) discarding: Mutex<HashSet<String>>,
    pub(crate) generated: Mutex<HashMap<String, u64>>,
    pub(crate) specials: Mutex<HashSet<String>>,
    pub(crate) sinks: Mutex<HashMap<String, (u64, String)>>,
    pub(crate) transferred: AtomicU64,
    /// Every `open` request: the path and the flags it came with.
    pub(crate) opened: Mutex<Vec<(String, russh_sftp::protocol::OpenFlags)>>,
    pub(crate) kill: Notify,
    /// Fired to end every connection with a reset (`RST`) instead of an orderly close.
    reset: Notify,
    /// How many requests are held (see [`SshdOptions::hold_realpath`]).
    pub(crate) held: AtomicUsize,
    pub(crate) held_notify: Notify,
    _temp: Option<tempfile::TempDir>,
}

pub struct TestSshd {
    addr: SocketAddr,
    state: Arc<State>,
    accept: tokio::task::JoinHandle<()>,
}

fn random_key() -> PrivateKey {
    PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("an ed25519 key can be generated")
}

impl TestSshd {
    pub fn start(options: SshdOptions) -> TestSshd {
        let allowed = PublicKey::from_openssh(&options.client_public).expect("client_public is an OpenSSH public key");
        let (root, temp) = match &options.root {
            Some(root) => (root.clone(), None),
            None => {
                let temp = tempfile::tempdir().expect("a temporary directory");
                let path = if options.openssh_sftp {
                    // OpenSSH reports its working directory without links in it.
                    temp.path().canonicalize().expect("the temporary directory exists")
                } else {
                    temp.path().to_owned()
                };
                (path, Some(temp))
            }
        };
        assert!(
            !options.openssh_sftp || Path::new(OPENSSH_SFTP_SERVER).is_file(),
            "the test needs OpenSSH's SFTP server at {OPENSSH_SFTP_SERVER}: install the package openssh-sftp-server"
        );
        let state = Arc::new(State {
            good: random_key(),
            bad: random_key(),
            allowed,
            options,
            connections: AtomicUsize::new(0),
            closed: Mutex::default(),
            closed_notify: Notify::new(),
            started: AtomicUsize::new(0),
            started_notify: Notify::new(),
            root,
            links: Mutex::default(),
            modes: Mutex::default(),
            read_only_dirs: Mutex::default(),
            failing_data: Mutex::default(),
            phantoms: Mutex::default(),
            unlookable: Mutex::default(),
            listed_again: Mutex::default(),
            specials: Mutex::default(),
            discarding: Mutex::default(),
            generated: Mutex::default(),
            sinks: Mutex::default(),
            transferred: AtomicU64::new(0),
            opened: Mutex::default(),
            kill: Notify::new(),
            reset: Notify::new(),
            held: AtomicUsize::new(0),
            held_notify: Notify::new(),
            _temp: temp,
        });
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        listener.set_nonblocking(true).expect("a nonblocking listener");
        let addr = listener.local_addr().expect("a bound address");
        let accept = runtime::spawn(accept_loop(listener, state.clone())).expect("the SSH runtime is running");
        TestSshd { addr, state, accept }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The host key the server presents, as OpenSSH text.
    pub fn host_public(&self) -> String {
        self.state.good.public_key().to_openssh().expect("a public key encodes")
    }

    /// The other host key, which the first `wrong_host_key_connections` connections present.
    pub fn wrong_host_public(&self) -> String {
        self.state.bad.public_key().to_openssh().expect("a public key encodes")
    }

    /// The directory the SFTP subsystem serves.
    pub fn root(&self) -> &Path {
        &self.state.root
    }

    /// How many connections were accepted.
    pub fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }

    /// Blocks until a client has closed a channel, which is what a command that is not waited for observes.
    pub fn wait_for_channel_close(&self) {
        runtime::wait(async {
            loop {
                let notified = self.state.closed_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !self.state.closed.lock().unwrap().is_empty() {
                    return;
                }
                notified.await;
            }
        })
    }

    /// Blocks until the server has received `count` `exec` commands in total: the commands are then in flight on the
    /// server, which is what a test that acts on a running command waits for.
    pub fn wait_for_commands(&self, count: usize) {
        runtime::wait(async {
            loop {
                let notified = self.state.started_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.state.started.load(Ordering::SeqCst) >= count {
                    return;
                }
                notified.await;
            }
        })
    }

    /// Blocks until the server holds a request (see [`SshdOptions::hold_realpath`]).
    pub fn wait_for_held_request(&self) {
        runtime::wait(async {
            loop {
                let notified = self.state.held_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.state.held.load(Ordering::SeqCst) > 0 {
                    return;
                }
                notified.await;
            }
        })
    }

    /// Drops every open connection without a word.
    pub fn drop_connections(&self) {
        self.state.kill.notify_waiters();
    }

    /// Resets every open connection (`RST`, as a peer that crashed or a firewall does), with no closing handshake.
    pub fn reset_connections(&self) {
        self.state.reset.notify_waiters();
    }

    /// `path` (a path in the served tree, `/`-separated) is a symlink to `target`.
    pub fn add_symlink(&self, path: &str, target: &str) {
        self.state
            .links
            .lock()
            .unwrap()
            .insert(path.to_owned(), target.to_owned());
    }

    /// The permission bits of `path`, as if a client had set them. A file without a write bit refuses to be opened for
    /// writing.
    pub fn set_mode(&self, path: &str, mode: u32) {
        self.state.modes.lock().unwrap().insert(path.to_owned(), mode);
    }

    /// What the symlink `path` points to, if it is one.
    pub fn link_target(&self, path: &str) -> Option<String> {
        self.state.links.lock().unwrap().get(path).cloned()
    }

    /// Every symlink as `(path, target)`, sorted by path.
    pub fn links(&self) -> Vec<(String, String)> {
        let mut links: Vec<_> = self
            .state
            .links
            .lock()
            .unwrap()
            .iter()
            .map(|(path, target)| (path.clone(), target.clone()))
            .collect();
        links.sort();
        links
    }

    /// The permission bits a client set on `path` when it created it.
    pub fn mode_of(&self, path: &str) -> Option<u32> {
        self.state.modes.lock().unwrap().get(path).copied()
    }

    /// Creating, removing and renaming entries in `dir` fail with permission denied; files already in it stay
    /// writable, as in a directory the user cannot write to.
    pub fn make_dir_read_only(&self, dir: &str) {
        self.state.read_only_dirs.lock().unwrap().insert(dir.to_owned());
    }

    /// A directory listing names `path`, which cannot be inspected: how a file whose name is not valid UTF-8 looks to
    /// a client that decodes names lossily.
    pub fn list_phantom(&self, path: &str) {
        self.state.phantoms.lock().unwrap().insert(path.to_owned());
    }

    /// Looking at `path` through a symlink is refused with `status`: OpenSSH's `sftp-server` answers permission denied
    /// for EPERM and EACCES, and bad message for ENAMETOOLONG and EINVAL.
    pub fn refuse_lookup(&self, path: &str, status: StatusCode) {
        self.state.unlookable.lock().unwrap().insert(path.to_owned(), status);
    }

    /// A directory listing names `path`, an entry that exists, a second time, at its end.
    pub fn list_again(&self, path: &str) {
        self.state.listed_again.lock().unwrap().insert(path.to_owned());
    }

    /// Reads and writes of the files in `dir` fail with a status error, as a full disk or a quota does.
    pub fn fail_data_in(&self, dir: &str) {
        self.state.failing_data.lock().unwrap().insert(dir.to_owned());
    }

    /// Files written in `dir` are hashed and dropped: see [`TestSshd::sink_result`].
    pub fn discard_uploads_in(&self, dir: &str) {
        self.state.discarding.lock().unwrap().insert(dir.to_owned());
    }

    /// The length and SHA-256 (hex) of what was uploaded to the discarded file `path`.
    pub fn sink_result(&self, path: &str) -> Option<(u64, String)> {
        self.state.sinks.lock().unwrap().get(path).cloned()
    }

    /// The path and the flags of every SFTP `open` the server has received, in order.
    pub fn opened(&self) -> Vec<(String, russh_sftp::protocol::OpenFlags)> {
        self.state.opened.lock().unwrap().clone()
    }

    /// `path` is neither a regular file, a directory nor a symlink (a FIFO, say).
    pub fn add_special_file(&self, path: &str) {
        self.state.specials.lock().unwrap().insert(path.to_owned());
    }

    /// `path` is a file of `size` bytes whose byte at offset `o` is [`pattern_byte`]`(o)`.
    pub fn add_generated_file(&self, path: &str, size: u64) {
        self.state.generated.lock().unwrap().insert(path.to_owned(), size);
    }
}

impl Drop for TestSshd {
    fn drop(&mut self) {
        self.accept.abort();
        // Wakes every connection, which then ends.
        self.state.kill.notify_waiters();
    }
}

async fn accept_loop(listener: std::net::TcpListener, state: Arc<State>) {
    let listener = TcpListener::from_std(listener).expect("the listener joins the runtime");
    let mut silent_streams = Vec::new();
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let index = state.connections.fetch_add(1, Ordering::SeqCst);
        if state.options.silent {
            // Held open until the server is dropped.
            silent_streams.push(stream);
            continue;
        }
        let host_key = if index < state.options.wrong_host_key_connections {
            state.bad.clone()
        } else {
            state.good.clone()
        };
        let config = Arc::new(Config {
            keys: vec![host_key],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Config::default()
        });
        let connection = Connection {
            state: state.clone(),
            index,
            channels: HashMap::new(),
            drop_now: Arc::new(Notify::new()),
            reading_stdin: HashSet::new(),
        };
        // The session reads and writes a pipe that a proxy task joins to the TCP stream, so ending the proxy drops the
        // connection without a word.
        let (server_end, mut proxy_end) = tokio::io::duplex(64 * 1024);
        let drop_now = connection.drop_now.clone();
        let shutdown = state.clone();
        let mut tcp = stream;
        runtime::spawn(async move {
            tokio::select! {
                _ = tokio::io::copy_bidirectional(&mut tcp, &mut proxy_end) => {}
                () = drop_now.notified() => {}
                () = shutdown.kill.notified() => {}
                () = shutdown.reset.notified() => {
                    // Closing with a zero linger time sends `RST`, not `FIN`: the very thing under test.
                    #[allow(deprecated, reason = "SO_LINGER is how a test asks for a reset")]
                    let _ = tcp.set_linger(Some(Duration::ZERO));
                }
            }
        })
        .expect("the SSH runtime is running");
        runtime::spawn(async move {
            if let Ok(running) = russh::server::run_stream(config, server_end, connection).await {
                let _ = running.await;
            }
        })
        .expect("the SSH runtime is running");
    }
}

struct Connection {
    state: Arc<State>,
    index: usize,
    channels: HashMap<ChannelId, Channel<Msg>>,
    /// Fired by the `drop-connection` script: the TCP connection is dropped without a word.
    drop_now: Arc<Notify>,
    /// Channels running `read-stdin`: they end when the client sends EOF.
    reading_stdin: HashSet<ChannelId>,
}

impl Handler for Connection {
    type Error = russh::Error;

    async fn auth_publickey(&mut self, _user: &str, key: VerifiedPublicKey<'_>) -> Result<Auth, Self::Error> {
        let accepted = self.index >= self.state.options.rejected_key_connections
            && key.public_key.key_data() == self.state.allowed.key_data();
        Ok(if accepted { Auth::Accept } else { Auth::reject() })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn channel_close(&mut self, channel: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        self.state.closed.lock().unwrap().insert(channel);
        self.state.closed_notify.notify_waiters();
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(sftp) = (name == "sftp").then(|| self.channels.remove(&channel)).flatten() else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        session.channel_success(channel)?;
        let state = self.state.clone();
        let drop_now = self.drop_now.clone();
        if state.options.openssh_sftp {
            runtime::spawn(serve_with_openssh(state, sftp.into_stream())).expect("the SSH runtime is running");
            return Ok(());
        }
        runtime::spawn(async move {
            let close_now = Arc::new(Notify::new());
            let server = fs::SftpFs::new(state.clone(), drop_now, close_now.clone());
            // The server reads and writes a pipe that a proxy task joins to the channel, so ending the proxy closes the
            // channel and nothing else.
            let (server_end, mut proxy_end) = tokio::io::duplex(64 * 1024);
            let mut channel = sftp.into_stream();
            runtime::spawn(async move {
                tokio::select! {
                    _ = tokio::io::copy_bidirectional(&mut channel, &mut proxy_end) => {}
                    () = close_now.notified() => {}
                }
            })
            .expect("the SSH runtime is running");
            russh_sftp::server::run(server_end, server).await;
        })
        .expect("the SSH runtime is running");
        Ok(())
    }

    async fn channel_eof(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        if self.reading_stdin.remove(&channel) {
            session.data(
                channel,
                b"stdin ended
"
                .to_vec(),
            )?;
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
        }
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        self.state.started.fetch_add(1, Ordering::SeqCst);
        self.state.started_notify.notify_waiters();
        let command = String::from_utf8_lossy(data).into_owned();
        let handle = session.handle();
        let words: Vec<&str> = command.split_whitespace().collect();
        match words.as_slice() {
            // Stops this connection's loop: nothing is read or written from now on.
            ["freeze"] => std::future::pending::<()>().await,
            ["drop-connection"] => {
                let _ = handle.data(channel, b"about to drop\n".to_vec()).await;
                self.drop_now.notify_one();
            }
            ["read-stdin"] => {
                self.reading_stdin.insert(channel);
            }
            _ => {
                let state = self.state.clone();
                runtime::spawn(script(state, handle, channel, command)).expect("the SSH runtime is running");
            }
        }
        Ok(())
    }
}

/// Serves SFTP on `channel` with OpenSSH's `sftp-server`, started in the served directory. The server ends when the
/// client closes the channel, and the channel closes when the server ends.
async fn serve_with_openssh(state: Arc<State>, channel: impl AsyncRead + AsyncWrite + Unpin) {
    let mut child = tokio::process::Command::new(OPENSSH_SFTP_SERVER)
        .current_dir(&state.root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("OpenSSH's sftp-server starts");
    let mut stdin = child.stdin.take().expect("a piped stdin");
    let mut stdout = child.stdout.take().expect("a piped stdout");
    let (mut from_client, mut to_client) = tokio::io::split(channel);
    let requests = async move {
        let _ = tokio::io::copy(&mut from_client, &mut stdin).await;
        // Dropped, not shut down: closing the pipe is what ends the server.
        drop(stdin);
    };
    let replies = async move {
        let _ = tokio::io::copy(&mut stdout, &mut to_client).await;
        let _ = to_client.shutdown().await;
    };
    tokio::join!(requests, replies);
}

/// The 256-byte-value pattern, `len` bytes of it from `offset` on; `reversed` counts down instead of up.
fn pattern_chunk(offset: u64, len: usize, reversed: bool) -> Vec<u8> {
    (0..len as u64)
        .map(|i| {
            let value = ((offset + i) % 256) as u8;
            if reversed { 255 - value } else { value }
        })
        .collect()
}

/// Writes `mib` MiB of the pattern to `channel` (extended data stream 1 when `stderr`).
async fn write_pattern(handle: &russh::server::Handle, channel: ChannelId, mib: u64, stderr: bool) {
    const CHUNK: usize = 64 * 1024;
    let total = mib * 1024 * 1024;
    let mut offset = 0;
    while offset < total {
        let len = CHUNK.min((total - offset) as usize);
        let bytes = pattern_chunk(offset, len, stderr);
        let sent = if stderr {
            handle.extended_data(channel, 1, bytes).await.is_ok()
        } else {
            handle.data(channel, bytes).await.is_ok()
        };
        if !sent {
            return;
        }
        offset += len as u64;
    }
}

async fn finish(handle: &russh::server::Handle, channel: ChannelId, status: u32) {
    let _ = handle.exit_status_request(channel, status).await;
    let _ = handle.eof(channel).await;
    let _ = handle.close(channel).await;
}

async fn script(state: Arc<State>, handle: russh::server::Handle, channel: ChannelId, command: String) {
    let words: Vec<&str> = command.split_whitespace().collect();
    match words.as_slice() {
        ["exit", n] => finish(&handle, channel, n.parse().expect("exit takes a number")).await,
        ["say-error-and-exit-125"] => {
            let _ = handle.data(channel, b"devvm-error: x: y\n".to_vec()).await;
            finish(&handle, channel, 125).await;
        }
        ["signal", name, rest @ ..] => {
            let sig = match *name {
                "SEGV" => Sig::SEGV,
                "KILL" => Sig::KILL,
                other => Sig::Custom(other.to_owned()),
            };
            let core = rest.first() == Some(&"core");
            let _ = handle
                .exit_signal_request(channel, sig, core, String::new(), String::new())
                .await;
            let _ = handle.close(channel).await;
        }
        ["close-no-status"] => {
            let _ = handle.close(channel).await;
        }
        ["pattern", mib] => {
            let mib = mib.parse().expect("pattern takes MiB");
            tokio::join!(
                write_pattern(&handle, channel, mib, false),
                write_pattern(&handle, channel, mib, true)
            );
            finish(&handle, channel, 0).await;
        }
        ["stdout-mib", mib] => {
            write_pattern(&handle, channel, mib.parse().expect("stdout-mib takes MiB"), false).await;
            finish(&handle, channel, 0).await;
        }
        ["write-forever"] => {
            let chunk = vec![b'y'; 1024];
            loop {
                if state.closed.lock().unwrap().contains(&channel) || handle.data(channel, chunk.clone()).await.is_err()
                {
                    return;
                }
            }
        }
        ["hold"] => {}
        ["status-then-marker"] => {
            let _ = handle.exit_status_request(channel, 7).await;
            let _ = handle
                .data(
                    channel,
                    b"marker
"
                    .to_vec(),
                )
                .await;
        }
        ["write-once-and-hold"] => {
            let _ = handle
                .data(
                    channel,
                    b"first
"
                    .to_vec(),
                )
                .await;
        }
        ["extended-data", stream] => {
            let stream = stream.parse().expect("extended-data takes a stream number");
            let _ = handle
                .extended_data(
                    channel,
                    stream,
                    b"on a stream of its own
"
                    .to_vec(),
                )
                .await;
            finish(&handle, channel, 0).await;
        }
        ["extended-data-and-hold", stream] => {
            let stream = stream.parse().expect("extended-data-and-hold takes a stream number");
            let _ = handle
                .extended_data(
                    channel,
                    stream,
                    b"on a stream of its own
"
                    .to_vec(),
                )
                .await;
        }
        ["status-then-extended-data", stream] => {
            let stream = stream.parse().expect("status-then-extended-data takes a stream number");
            let _ = handle
                .data(
                    channel,
                    b"before the status
"
                    .to_vec(),
                )
                .await;
            let _ = handle.exit_status_request(channel, 7).await;
            let _ = handle
                .extended_data(
                    channel,
                    stream,
                    b"on a stream of its own
"
                    .to_vec(),
                )
                .await;
            let _ = handle.close(channel).await;
        }
        ["echo", rest @ ..] => {
            let _ = handle.data(channel, format!("{}\n", rest.join(" ")).into_bytes()).await;
            finish(&handle, channel, 0).await;
        }
        _ => {
            let _ = handle
                .extended_data(
                    channel,
                    1,
                    format!("test sshd: unknown script `{command}`\n").into_bytes(),
                )
                .await;
            finish(&handle, channel, 127).await;
        }
    }
}

/// Short enough that a frozen peer is found in well under a second.
pub const FAST_KEEPALIVE: Keepalive = Keepalive {
    interval: Duration::from_millis(100),
    max_missed: 2,
};

/// A server together with the keys it accepts, and what a client needs to reach it.
pub struct Rig {
    pub sshd: TestSshd,
    pub keys: InstanceKeys,
    pub serial_log: PathBuf,
}

impl Rig {
    /// A server for fresh keys; `home` holds the serial log the errors name.
    pub fn new(home: &Path) -> Rig {
        Rig::with(home, |_| {})
    }

    pub fn with(home: &Path, configure: impl FnOnce(&mut SshdOptions)) -> Rig {
        let keys = InstanceKeys::generate().unwrap();
        let mut options = SshdOptions {
            client_public: keys.client_public.clone(),
            ..SshdOptions::default()
        };
        configure(&mut options);
        Rig {
            sshd: TestSshd::start(options),
            keys,
            serial_log: home.join("serial.log"),
        }
    }

    pub fn target(&self) -> SshTarget {
        SshTarget {
            addr: self.sshd.addr(),
            user: "user".to_owned(),
            client_private: self.keys.client_private.clone(),
            host_public: self.sshd.host_public(),
            serial_log: self.serial_log.clone(),
        }
    }

    pub fn connect_with(&self, vm: VmExit, boot: Option<BootBound>) -> Result<SshSession, Error> {
        SshSession::connect(&self.target(), FAST_KEEPALIVE, vm, boot, &NullProgress)
    }

    /// A session with no boot bound and no VM to watch.
    pub fn connect(&self) -> SshSession {
        self.connect_with(VmExit::never(), None)
            .expect("the test server accepts the client")
    }
}
