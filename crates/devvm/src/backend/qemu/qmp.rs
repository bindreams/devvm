//! A QMP client over an AF_UNIX socket.
//!
//! Commands and their replies use the `qapi` crate's types, so a schema change shows up as a parse error naming the
//! command. The line protocol is read here and not through the crate's session type: that one fails on an event its
//! schema does not know, and QEMU sends events (`RTC_CHANGE`, `NIC_RX_FILTER_CHANGED`, ...) that devvm never asks for.
//! The events devvm reads (`STOP`, `RESUME`, `RESET`, `SHUTDOWN`, `POWERDOWN`) are parsed with the crate's `Event`.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use qapi::qmp::{self, QapiCapabilities};
use qapi::{Command, Execute};

use super::ports::{Forward, parse_usernet};
use super::uds::UnixStream;
use crate::backend::FileClaim;
use crate::exit::Progress;
use crate::host::{Comparison, Hold, Host, Link, ProcessWatch, symlink_metadata_if_present};
use crate::random;
use crate::store::lock::{FileLock, Mode};
use crate::{Arch, Error, ErrorKind, Result};

#[cfg(test)]
#[path = "qmp_tests.rs"]
pub(crate) mod qmp_tests;

#[cfg(all(test, windows))]
#[path = "qmp_windows_tests.rs"]
mod qmp_windows_tests;

// Endpoint ============================================================================================================

/// The bytes a launch's name is the hex of.
const DRAW_BYTES: usize = 8;

const SOCKET_SUFFIX: &str = ".sock";
const LOCK_SUFFIX: &str = ".lock";
const HOLD_SUFFIX: &str = ".hold";

/// Which of a launch's three files a name is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchFileKind {
    Socket,
    Lock,
    Claim,
}

/// Where a machine's QMP socket is, and the files beside it.
///
/// A launch's names are `<run>/<16 hex>.sock` (QEMU makes it), `<name>.lock` (the lock of QMP sessions) and
/// `<name>.hold` (the launch's claim on the name, held by QEMU while it runs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QmpEndpoint {
    path: PathBuf,
}

/// What a launch has reserved: its names and the hold on the claim that keeps them its own until the machine runs.
#[derive(Debug)]
pub(crate) struct Reservation {
    pub endpoint: QmpEndpoint,
    /// The hold file as made. It goes in the machine's record; cleanup deletes nothing that it does not still name.
    pub claim: FileClaim,
    /// The hold on the claim file. QEMU inherits a duplicate of it; this one is dropped once QEMU is spawned.
    pub hold: Box<dyn Hold>,
}

/// The longest socket path a launch in `run` can draw, in the bytes that `sun_path` counts.
fn longest_socket_path(run: &Path) -> usize {
    QmpEndpoint::drawn(run, [0; DRAW_BYTES])
        .path
        .as_os_str()
        .as_encoded_bytes()
        .len()
}

impl QmpEndpoint {
    /// Whether every socket path in `run` fits `path_max` bytes (`Host::socket_path_max`).
    pub(crate) fn fits(run: &Path, path_max: usize) -> bool {
        longest_socket_path(run) <= path_max
    }

    /// The refusals of a launch that do not depend on the draw: a directory whose sockets would be over the host's
    /// limit (`Unavailable`, naming the directory, the limit and `XDG_RUNTIME_DIR`), and one that cannot be made.
    /// Creates `run` if it is not there. Idempotent: a launch calls it before it downloads anything, and `reserve`
    /// calls it again.
    pub(crate) fn prepare_run_dir(host: &dyn Host, run: &Path) -> Result<()> {
        let max = host.socket_path_max();
        if !QmpEndpoint::fits(run, max) {
            let bytes = longest_socket_path(run);
            return Err(Error::new(
                ErrorKind::Unavailable,
                format!(
                    "the QEMU control socket paths in {} would be {bytes} bytes, over the {max}-byte limit of an \
                     AF_UNIX path; set XDG_RUNTIME_DIR to a shorter directory",
                    run.display()
                ),
            ));
        }
        host.create_runtime_dir(run).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot use {} for QEMU's control sockets", run.display()),
                e,
            )
        })?;
        Ok(())
    }

    /// Reserves the names of a launch in the runtime directory `run`, which is made if it is not there: draws
    /// `<16 hex>.sock` with the OS's random numbers, and holds the claim file `<name>.sock.hold` that it creates
    /// (`create_new`), so no two launches have one name.
    /// A wait for a sweep of the directory is said to `progress`.
    pub(crate) fn reserve(host: &dyn Host, run: &Path, progress: &dyn Progress) -> Result<Reservation> {
        QmpEndpoint::reserve_with(host, run, progress, &mut random::bytes::<DRAW_BYTES>)
    }

    /// `reserve` with the draw given, to test a redraw.
    ///
    /// Refused first, creating nothing: a directory whose sockets would be over the host's limit (`Unavailable`,
    /// naming the directory, the limit and `XDG_RUNTIME_DIR`). A name whose claim file exists is drawn again, and so is
    /// one whose socket or lock file exists (left by a machine that is gone, and not this launch's to delete). Those
    /// two files are looked for before the claim is made, so no claim is ever made beside a file it did not make. A
    /// socket cannot appear at the name afterwards: only a QEMU spawned with the claim makes one, and this launch made
    /// it. A lock file can: a session of a dead machine that had the name makes it (`Qmp::connect`), even one that is
    /// then refused. The launch's cleanup and the sweep delete it.
    pub(crate) fn reserve_with(
        host: &dyn Host,
        run: &Path,
        progress: &dyn Progress,
        draw: &mut dyn FnMut() -> [u8; DRAW_BYTES],
    ) -> Result<Reservation> {
        QmpEndpoint::prepare_run_dir(host, run)?;
        // Shared while names are drawn and claimed, so no sweep runs between a claim file's creation and its hold.
        // Released on return: the hold covers the launch from then on, and a sweep finds it `Busy`.
        // Not `FileLock::open`: only `create_runtime_dir` makes the directory, with its owner and access set in one
        // call.
        let mut directory = FileLock::open_in_existing_dir(&run.join("runtime.lock"))?;
        directory.lock(Mode::Shared, "a sweep of the runtime directory", &mut |m| {
            progress.message(m)
        })?;
        'drawing: loop {
            let endpoint = QmpEndpoint::drawn(run, draw());
            for other in [endpoint.path().to_path_buf(), endpoint.lock_path()] {
                let found = symlink_metadata_if_present(&other)
                    .map_err(|e| Error::io(ErrorKind::Io, format!("cannot look at {}", other.display()), e))?;
                if found.is_some() {
                    continue 'drawing;
                }
            }
            let hold_path = endpoint.hold_path();
            let hold = match host.hold_new(&hold_path) {
                Ok(hold) => hold,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(Error::io(
                        ErrorKind::Io,
                        format!("cannot create {}", hold_path.display()),
                        e,
                    ));
                }
            };
            // The claim file goes on this path too: nothing would reclaim it but the next sweep.
            let id = match hold.id() {
                Ok(id) => id,
                Err(e) => {
                    let error = Error::io(ErrorKind::Io, format!("cannot look at {}", hold_path.display()), e);
                    return Err(error.after_cleanup(&hold_path.display().to_string(), hold.delete()));
                }
            };
            return Ok(Reservation {
                claim: FileClaim { path: hold_path, id },
                endpoint,
                hold,
            });
        }
    }

    /// The endpoint of the launch whose name is the hex of `draw`, in `run`.
    pub(crate) fn drawn(run: &Path, draw: [u8; DRAW_BYTES]) -> QmpEndpoint {
        QmpEndpoint::of_launch(run, &hex::encode(draw))
    }

    /// The endpoint of the launch named `launch` (16 hex digits), in `run`.
    pub(crate) fn of_launch(run: &Path, launch: &str) -> QmpEndpoint {
        QmpEndpoint::from_path(run.join(format!("{launch}{SOCKET_SUFFIX}")))
    }

    /// The launch (its 16 lowercase hex digits) and the kind of file, if `file_name` is one of a launch's three
    /// files: the inverse of [`QmpEndpoint::drawn`], [`QmpEndpoint::lock_path`] and [`QmpEndpoint::hold_path`].
    pub(crate) fn launch_file(file_name: &str) -> Option<(&str, LaunchFileKind)> {
        let (stem, kind) = if let Some(stem) = file_name.strip_suffix(HOLD_SUFFIX) {
            (stem, LaunchFileKind::Claim)
        } else if let Some(stem) = file_name.strip_suffix(LOCK_SUFFIX) {
            (stem, LaunchFileKind::Lock)
        } else {
            (file_name, LaunchFileKind::Socket)
        };
        let launch = stem.strip_suffix(SOCKET_SUFFIX)?;
        random::is_hex(launch, DRAW_BYTES).then_some((launch, kind))
    }

    /// The files of the launch, the claim last: it keeps the name reserved while the others are deleted.
    pub(crate) fn launch_files(&self) -> [PathBuf; 3] {
        [self.path.clone(), self.lock_path(), self.hold_path()]
    }

    /// The endpoint whose socket is at `path`.
    pub(crate) fn from_path(path: PathBuf) -> QmpEndpoint {
        QmpEndpoint { path }
    }

    /// The endpoint a `MachineRecord.qmp` names.
    pub(crate) fn from_record(qmp: &str) -> QmpEndpoint {
        QmpEndpoint::from_path(PathBuf::from(qmp))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// `<socket path>.lock`: held by whoever talks to the socket, so that QMP use is serialized across processes.
    pub(crate) fn lock_path(&self) -> PathBuf {
        self.beside(LOCK_SUFFIX)
    }

    /// `<socket path>.hold`: the launch's claim, held by QEMU for as long as it runs.
    pub(crate) fn hold_path(&self) -> PathBuf {
        self.beside(HOLD_SUFFIX)
    }

    fn beside(&self, suffix: &str) -> PathBuf {
        let mut name: OsString = self.path.clone().into_os_string();
        name.push(suffix);
        PathBuf::from(name)
    }
}

// Messages ============================================================================================================

/// The events devvm reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum QmpEvent {
    Stop,
    Resume,
    /// `requested`: the reason is the QMP `system_reset` devvm sent (`host-qmp-system-reset`), not the guest's own.
    Reset {
        requested: bool,
    },
    /// `requested`: as for `Reset`; under `-no-reboot` QEMU answers the reset with this and exits.
    Shutdown {
        requested: bool,
    },
    Powerdown,
    /// Any other event, by name.
    Other(String),
}

/// Why `wait_for_end` returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VmEnd {
    /// QEMU closed the socket: the process is exiting.
    Exited,
    /// QEMU stopped the guest and is not exiting; the QEMU run state is `.0` (`paused`, `io-error`, ...).
    Paused(String),
}

/// QEMU's run state, as `query-status` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunStatus {
    /// The QEMU run state name: `running`, `paused`, `io-error`, ...
    pub state: String,
}

impl RunStatus {
    /// Whether the state is one where a guest powers down or is on its way out by itself.
    pub(crate) fn is_settled_for_powerdown(&self) -> bool {
        matches!(self.state.as_str(), "running" | "suspended" | "shutdown")
    }
}

// Session -------------------------------------------------------------------------------------------------------------

/// A negotiated QMP session. It holds the endpoint's lock file for its life.
pub(crate) struct Qmp {
    /// The connection, read through a buffer; writes go to the link inside it.
    reader: BufReader<Box<dyn Link>>,
    events: VecDeque<QmpEvent>,
    endpoint: QmpEndpoint,
    process: Arc<dyn ProcessWatch>,
    _lock: FileLock,
}

impl Qmp {
    /// Connects to the socket, negotiates, and holds the lock file. It waits for the QEMU process to listen for as long
    /// as `process` has not exited, trying again every few milliseconds: the listener is up about 30 ms after spawn. A
    /// process that has exited fails before a lock file is touched. The socket's directory must exist: only a launch
    /// makes the runtime directory (`Host::create_runtime_dir`), and a missing one is an error that names it.
    ///
    /// The lock is waited for as long as the process has not exited. Once the socket has accepted, the file at
    /// `claim`'s path is compared with `claim`; if it is another launch's file, or none, the connection is dropped
    /// unread and the error is `ConnectionLost`: a running QEMU holds its claim, and on Windows a held claim cannot be
    /// removed by name, so the machine is over or ending. This checks the claim's identity, not the process's
    /// liveness: the hold is free before the exit is visible, so a sweep can free the name while a dying machine is
    /// still listed. Only a draw of the same 64 bits then gives the name to another launch, and the check is what makes
    /// that harmless. Nobody removes a running machine's claim by hand: where that is possible (Unix), the callers wait
    /// for the process on `ConnectionLost`, and so wait until QEMU ends.
    ///
    /// Never deletes the lock file, whether it succeeds or not: another session may be waiting on that very file, and a
    /// path removed under it could be a later session's. The file is removed by whoever knows no session can be live,
    /// the machine's shutdown or kill.
    ///
    /// A read on the session ends once the process has exited and QEMU's bytes are used up. On Windows the exit ends
    /// the read even if the OS never delivers the end of the stream; elsewhere the read ends with the stream, so a
    /// process that keeps the socket open after QEMU exits keeps the read waiting.
    pub(crate) fn connect(
        host: &dyn Host,
        ep: &QmpEndpoint,
        claim: &FileClaim,
        process: &Arc<dyn ProcessWatch>,
        progress: &dyn Progress,
    ) -> Result<Qmp> {
        let exited = |when: &str| {
            Error::new(
                ErrorKind::ConnectionLost,
                format!(
                    "cannot reach QEMU at {}: the QEMU process exited {when}",
                    ep.path().display()
                ),
            )
        };
        if process.has_exited()? {
            return Err(exited("before devvm connected"));
        }
        let mut lock = FileLock::open_in_existing_dir(&ep.lock_path())?;
        let what = format!(
            "another devvm process to finish with the QEMU control socket {}",
            ep.path().display()
        );
        if !lock.lock_while(
            Mode::Exclusive,
            &what,
            &mut |m| progress.message(m),
            Some(&mut || process.pause()),
        )? {
            return Err(exited("while another devvm process held the control socket"));
        }
        let stream = loop {
            match UnixStream::connect(ep.path()) {
                Ok(stream) => break stream,
                Err(e) if is_not_listening(&e) => match process.pause() {
                    Ok(false) => {}
                    Ok(true) => return Err(exited("before its control socket accepted a connection")),
                    Err(e) => return Err(e),
                },
                Err(e) => {
                    return Err(Error::io(
                        ErrorKind::Io,
                        format!("cannot connect to the QEMU control socket {}", ep.path().display()),
                        e,
                    ));
                }
            }
        };
        let state = claim
            .id
            .compare(host.file_id(&claim.path))
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot look at {}", claim.path.display()), e))?;
        let is = match state {
            Comparison::Same => None,
            Comparison::Different => Some("another launch's file"),
            Comparison::Absent => Some("gone"),
        };
        if let Some(is) = is {
            // The stream and the lock go with this return, unread.
            return Err(Error::new(
                ErrorKind::ConnectionLost,
                format!(
                    "cannot reach QEMU at {}: claim file {} is {is}: a running QEMU holds its claim, so the machine \
                     is over or ending",
                    ep.path().display(),
                    claim.path.display()
                ),
            ));
        }
        let link = process.link(stream).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot watch the connection to {}", ep.path().display()),
                e,
            )
        })?;
        let mut qmp = Qmp {
            reader: BufReader::new(link),
            events: VecDeque::new(),
            endpoint: ep.clone(),
            process: process.clone(),
            _lock: lock,
        };
        qmp.negotiate()?;
        Ok(qmp)
    }

    fn lost(&self, what: &str) -> Error {
        Error::new(
            ErrorKind::ConnectionLost,
            format!(
                "QEMU closed its control socket {} while devvm was waiting for {what}",
                self.endpoint.path().display(),
            ),
        )
    }

    /// Whether the QEMU process is known to have exited.
    fn exited(&self) -> bool {
        matches!(self.process.has_exited(), Ok(true))
    }

    fn negotiate(&mut self) -> Result<()> {
        let greeting = self.read_value()?.ok_or_else(|| self.lost("its greeting"))?;
        serde_json::from_value::<QapiCapabilities>(greeting.clone()).map_err(|e| {
            Error::new(
                ErrorKind::Internal,
                format!("QEMU's greeting is not a QMP greeting ({e}): {greeting}"),
            )
        })?;
        self.execute(&qmp::qmp_capabilities { enable: None }).map(drop)
    }

    // Wire ------------------------------------------------------------------------------------------------------------

    fn read_value(&mut self) -> Result<Option<serde_json::Value>> {
        let mut line = String::new();
        let read = match self.reader.read_line(&mut line) {
            Ok(read) => read,
            // A QEMU that exits resets the connection on Windows instead of closing it: nothing more comes. Any other
            // error, a corrupt line included, is reported as itself even when QEMU has exited since.
            Err(e) if is_closed(&e) || (self.exited() && e.kind() != std::io::ErrorKind::InvalidData) => 0,
            Err(e) => {
                return Err(Error::io(
                    ErrorKind::Io,
                    format!(
                        "cannot read from the QEMU control socket {}",
                        self.endpoint.path().display()
                    ),
                    e,
                ));
            }
        };
        if !is_whole_line(read, &line) {
            return Ok(None);
        }
        serde_json::from_str(&line).map(Some).map_err(|e| {
            Error::new(
                ErrorKind::Internal,
                format!("QEMU sent a line that is not JSON ({e}): {}", line.trim_end()),
            )
        })
    }

    /// Reads one message. An event goes to the queue and `None` comes back; a response comes back as `Some`.
    fn read_message(&mut self) -> Result<Option<Option<serde_json::Value>>> {
        let Some(value) = self.read_value()? else {
            return Ok(None);
        };
        if value.get("event").is_some() {
            let event = classify(&value)?;
            self.events.push_back(event);
            Ok(Some(None))
        } else {
            Ok(Some(Some(value)))
        }
    }

    fn execute<C: Command>(&mut self, command: &C) -> Result<C::Ok> {
        let line = serde_json::to_string(&Execute::<&C>::from(command)).map_err(|e| {
            Error::new(
                ErrorKind::Internal,
                format!("cannot encode the QMP command {}: {e}", C::NAME),
            )
        })?;
        let link = self.reader.get_mut();
        link.write_all(line.as_bytes())
            .and_then(|()| link.write_all(b"\n"))
            .and_then(|()| link.flush())
            .map_err(|e| {
                Error::io(
                    if is_closed(&e) || self.exited() {
                        ErrorKind::ConnectionLost
                    } else {
                        ErrorKind::Io
                    },
                    format!(
                        "cannot send {} to the QEMU control socket {}",
                        C::NAME,
                        self.endpoint.path().display(),
                    ),
                    e,
                )
            })?;
        let what = format!("the reply to {}", C::NAME);
        let value = loop {
            match self.read_message()? {
                None => return Err(self.lost(&what)),
                Some(None) => continue,
                Some(Some(value)) => break value,
            }
        };
        let message: qmp::QmpMessage<C::Ok> = serde_json::from_value(value.clone()).map_err(|e| {
            Error::new(
                ErrorKind::Internal,
                format!(
                    "QEMU's reply to {} does not fit the qapi schema ({e}): {value}",
                    C::NAME
                ),
            )
        })?;
        let qmp::QmpMessage::Response(response) = message else {
            return Err(Error::new(
                ErrorKind::Internal,
                format!("QEMU's reply to {} is an event: {value}", C::NAME),
            ));
        };
        response
            .result()
            .map_err(|e| Error::new(ErrorKind::Internal, format!("QEMU refused {}: {}", C::NAME, e.desc)))
    }

    // Commands --------------------------------------------------------------------------------------------------------

    /// Asks the guest to power down (ACPI). `Ok` means QEMU acknowledged the request; the guest may or may not act on
    /// it. An error says nothing of whether QEMU saw or acknowledged it: an acknowledgement may have been sent and
    /// not read.
    pub(crate) fn system_powerdown(&mut self) -> Result<()> {
        self.execute(&qmp::system_powerdown {}).map(drop)
    }

    /// Makes QEMU exit now. The socket closes after the reply.
    pub(crate) fn quit(&mut self) -> Result<()> {
        self.execute(&qmp::quit {}).map(drop)
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        self.execute(&qmp::stop {}).map(drop)
    }

    pub(crate) fn cont(&mut self) -> Result<()> {
        self.execute(&qmp::cont {}).map(drop)
    }

    /// Resets the guest inside this QEMU process and returns once QEMU has reported the `RESET` event this request
    /// caused (reason `host-qmp-system-reset`), not on the reply alone and not on a reset the guest did by itself. A
    /// QEMU started with `-no-reboot` shuts down instead and closes the socket; that is an error here.
    pub(crate) fn system_reset(&mut self) -> Result<()> {
        self.events
            .retain(|e| !matches!(e, QmpEvent::Reset { .. } | QmpEvent::Shutdown { .. }));
        self.execute(&qmp::system_reset {})?;
        loop {
            if let Some(index) = self
                .events
                .iter()
                .position(|e| *e == QmpEvent::Reset { requested: true })
            {
                self.events.remove(index);
                return Ok(());
            }
            if self.events.contains(&QmpEvent::Shutdown { requested: true }) {
                return Err(Error::new(
                    ErrorKind::ConnectionLost,
                    "QEMU shut down instead of resetting the guest (it runs with -no-reboot)",
                ));
            }
            if self.read_message()?.is_none() {
                return Err(self.lost("the RESET event of system_reset"));
            }
        }
    }

    pub(crate) fn query_status(&mut self) -> Result<RunStatus> {
        let status = self.execute(&qmp::query_status {})?;
        let state = serde_json::to_value(status.status)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| Error::new(ErrorKind::Internal, "QEMU's run state has no name"))?;
        Ok(RunStatus { state })
    }

    /// The architecture this QEMU emulates (`query-target`). A machine found again from its record has no other way to
    /// say what it is.
    pub(crate) fn target_arch(&mut self) -> Result<Arch> {
        let target = self.execute(&qmp::query_target {})?;
        let name = serde_json::to_value(target.arch)
            .map_err(|e| {
                Error::new(
                    ErrorKind::Internal,
                    format!("cannot read the architecture QEMU reported ({e}): {:?}", target.arch),
                )
            })?
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Internal,
                    format!("QEMU's architecture has no name: {:?}", target.arch),
                )
            })?;
        match name.as_str() {
            "x86_64" => Ok(Arch::Amd64),
            "aarch64" => Ok(Arch::Arm64),
            other => Err(Error::new(
                ErrorKind::Unavailable,
                format!("QEMU emulates {other:?}, which devvm has no guest for"),
            )),
        }
    }

    /// Fails with `Unavailable` unless the run state is one of `acceptable`, naming the state and the serial log: a
    /// paused VM ignores a powerdown and a reset, and a caller must not wait for either.
    pub(crate) fn require_state(&mut self, acceptable: &[&str], action: &str, serial_log: &Path) -> Result<()> {
        let status = self.query_status()?;
        if acceptable.contains(&status.state.as_str()) {
            return Ok(());
        }
        Err(Error::new(
            ErrorKind::Unavailable,
            format!(
                "cannot {action}: the VM is {}, not {}; its serial log is {}",
                status.state,
                acceptable.join(" or "),
                serial_log.display()
            ),
        ))
    }

    /// The `hostfwd` rules and the ports QEMU bound for them (HMP `info usernet`, the only place a port-0 forward is
    /// readable).
    pub(crate) fn forwarded_ports(&mut self) -> Result<Vec<Forward>> {
        let text = self.execute(&qmp::human_monitor_command {
            cpu_index: None,
            command_line: "info usernet".to_owned(),
        })?;
        parse_usernet(&text)
    }

    /// Blocks until the VM is over, reading the connection and nothing else (no timer, no polling): `Exited` when QEMU
    /// closes the socket, `Paused` when a `STOP` event leaves the VM in a state other than a normal one.
    pub(crate) fn wait_for_end(&mut self) -> Result<VmEnd> {
        loop {
            while let Some(event) = self.events.pop_front() {
                if event == QmpEvent::Stop {
                    let status = self.query_status()?;
                    if !status.is_settled_for_powerdown() {
                        return Ok(VmEnd::Paused(status.state));
                    }
                }
            }
            if self.read_message()?.is_none() {
                return Ok(VmEnd::Exited);
            }
        }
    }
}

/// Whether a read produced a message: nothing read is the end of the stream, and so is a line cut off before its
/// newline, which is what a QEMU that died in the middle of a write leaves.
fn is_whole_line(read: usize, line: &str) -> bool {
    read > 0 && line.ends_with('\n')
}

/// Whether a failed read means the peer has gone away.
fn is_closed(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::BrokenPipe
    )
}

/// Whether a failed connect means "nobody listens yet" and not "this can never work": no socket, a socket nobody
/// accepts on, or an attempt a starting or dying QEMU reset or aborted.
fn is_not_listening(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    ) || is_closed(error)
}

/// The event `value` is. The events devvm reads must parse as the `qapi` crate's `Event`; any other is kept by name.
fn classify(value: &serde_json::Value) -> Result<QmpEvent> {
    let name = value["event"].as_str().unwrap_or_default();
    if !matches!(name, "STOP" | "RESUME" | "RESET" | "SHUTDOWN" | "POWERDOWN") {
        return Ok(QmpEvent::Other(name.to_owned()));
    }
    let event = serde_json::from_value::<qmp::Event>(value.clone()).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            format!("QEMU's {name} event does not fit the qapi schema ({e}): {value}"),
        )
    })?;
    // The reason tells the reset devvm asked for from one the guest did by itself.
    let requested = |reason: qmp::ShutdownCause| reason == qmp::ShutdownCause::host_qmp_system_reset;
    Ok(match event {
        qmp::Event::STOP { .. } => QmpEvent::Stop,
        qmp::Event::RESUME { .. } => QmpEvent::Resume,
        qmp::Event::RESET { data, .. } => QmpEvent::Reset {
            requested: requested(data.reason),
        },
        qmp::Event::SHUTDOWN { data, .. } => QmpEvent::Shutdown {
            requested: requested(data.reason),
        },
        qmp::Event::POWERDOWN { .. } => QmpEvent::Powerdown,
        _ => QmpEvent::Other(name.to_owned()),
    })
}
