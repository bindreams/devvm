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

use qapi::qmp::{self, QapiCapabilities};
use qapi::{Command, Execute};

use super::exit_watch::{End, ExitWatch, Registration};
use super::ports::{Forward, parse_usernet};
use super::uds::UnixStream;
use crate::exit::Progress;
use crate::instance::record::InstanceId;
use crate::store::lock::{FileLock, Mode};
use crate::{Arch, Error, ErrorKind, Result};

#[cfg(test)]
#[path = "qmp_tests.rs"]
pub(crate) mod qmp_tests;

/// The longest socket path in UTF-8 bytes: `sun_path` is 108 bytes with its terminating NUL.
const SUN_PATH_MAX: usize = 107;

// Endpoint ============================================================================================================

/// Where a machine's QMP socket is, and the lock file beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QmpEndpoint {
    path: PathBuf,
}

impl QmpEndpoint {
    /// `<temp>/dvm-<32 hex of the id>.sock`: short, so it fits `sun_path` under any temp directory, and unique per
    /// instance. A path over the limit is refused here, before a machine is launched with it.
    pub(crate) fn for_instance(temp: &Path, id: &InstanceId) -> Result<QmpEndpoint> {
        let path = temp.join(format!("dvm-{id}.sock"));
        let bytes = path.to_string_lossy().len();
        if bytes > SUN_PATH_MAX {
            return Err(Error::new(
                ErrorKind::Unavailable,
                format!(
                    "the QEMU control socket path {} is {bytes} bytes, over the {SUN_PATH_MAX}-byte limit of an AF_UNIX \
                     path; use a shorter temporary directory (TMP or TEMP)",
                    path.display()
                ),
            ));
        }
        Ok(QmpEndpoint { path })
    }

    /// The endpoint a `MachineRecord.qmp` names.
    pub(crate) fn from_record(qmp: &str) -> QmpEndpoint {
        QmpEndpoint {
            path: PathBuf::from(qmp),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// `<socket path>.lock`: held by whoever talks to the socket, so that QMP use is serialized across processes.
    pub(crate) fn lock_path(&self) -> PathBuf {
        let mut name: OsString = self.path.clone().into_os_string();
        name.push(".lock");
        PathBuf::from(name)
    }
}

// Messages ============================================================================================================

/// The events devvm reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum QmpEvent {
    Stop,
    Resume,
    Reset,
    Shutdown,
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
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    events: VecDeque<QmpEvent>,
    endpoint: QmpEndpoint,
    /// Declared before the lock so that it lets go of the connection before the lock is released.
    registration: Registration,
    _lock: FileLock,
}

fn lost(endpoint: &QmpEndpoint, what: &str) -> Error {
    Error::new(
        ErrorKind::ConnectionLost,
        format!(
            "QEMU closed its control socket {} while devvm was waiting for {what}",
            endpoint.path().display()
        ),
    )
}

impl Qmp {
    /// Connects to the socket, negotiates, and holds the lock file. It waits for the QEMU process to listen for as long
    /// as `exit` does not report the process gone, trying again every few milliseconds: the listener is up about 30 ms
    /// after spawn. The process exiting ends the wait, and also any later read or write on the session. A machine
    /// that has exited fails before a lock file is touched.
    pub(crate) fn connect(ep: &QmpEndpoint, exit: &ExitWatch, progress: &dyn Progress) -> Result<Qmp> {
        let stopped = |end: End, when: &str| match end {
            End::Exited => Error::new(
                ErrorKind::ConnectionLost,
                format!(
                    "cannot reach QEMU at {}: the QEMU process exited {when}",
                    ep.path().display()
                ),
            ),
            End::Unwatchable(why) => Error::new(
                ErrorKind::Io,
                format!("cannot wait for the QEMU process at {}: {why}", ep.path().display()),
            ),
        };
        if let Some(end) = exit.ended() {
            return Err(stopped(end, "before devvm connected"));
        }
        let mut lock = FileLock::open(&ep.lock_path())?;
        lock.lock(
            Mode::Exclusive,
            &format!(
                "another devvm process to finish with the QEMU control socket {}",
                ep.path().display()
            ),
            progress,
        )?;
        let stream = loop {
            match UnixStream::connect(ep.path()) {
                Ok(stream) => break stream,
                Err(e) if is_not_listening(&e) => {
                    if let Some(end) = exit.pause() {
                        return Err(stopped(end, "before its control socket accepted a connection"));
                    }
                }
                Err(e) => {
                    return Err(Error::io(
                        ErrorKind::Io,
                        format!("cannot connect to the QEMU control socket {}", ep.path().display()),
                        e,
                    ));
                }
            }
        };
        let duplicate = |what: &str| {
            stream.try_clone().map_err(|e| {
                Error::io(
                    ErrorKind::Io,
                    format!("cannot duplicate the connection to {} {what}", ep.path().display()),
                    e,
                )
            })
        };
        let reader = duplicate("for reading")?;
        let registration = exit
            .register(duplicate("for the exit watch")?)
            .map_err(|end| stopped(end, "while devvm was connecting"))?;
        let mut qmp = Qmp {
            reader: BufReader::new(reader),
            writer: stream,
            events: VecDeque::new(),
            endpoint: ep.clone(),
            registration,
            _lock: lock,
        };
        qmp.negotiate()?;
        Ok(qmp)
    }

    fn negotiate(&mut self) -> Result<()> {
        let greeting = self.read_value()?.ok_or_else(|| lost(&self.endpoint, "its greeting"))?;
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
            // A QEMU that exits resets the connection on Windows instead of closing it, and the exit watch shuts it
            // down: either way, it is gone.
            Err(e) if is_closed(&e) || self.registration.exited() => 0,
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
        self.writer
            .write_all(line.as_bytes())
            .and_then(|()| self.writer.write_all(b"\n"))
            .and_then(|()| self.writer.flush())
            .map_err(|e| {
                Error::io(
                    if is_closed(&e) || self.registration.exited() {
                        ErrorKind::ConnectionLost
                    } else {
                        ErrorKind::Io
                    },
                    format!(
                        "cannot send {} to the QEMU control socket {}",
                        C::NAME,
                        self.endpoint.path().display()
                    ),
                    e,
                )
            })?;
        let what = format!("the reply to {}", C::NAME);
        let value = loop {
            match self.read_message()? {
                None => return Err(lost(&self.endpoint, &what)),
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

    /// Asks the guest to power down (ACPI). The guest may or may not act on it; this only sends the request.
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

    /// Resets the guest inside this QEMU process and returns once QEMU has reported the `RESET` event, not on the reply
    /// alone. A QEMU started with `-no-reboot` shuts down instead and closes the socket; that is an error here.
    pub(crate) fn system_reset(&mut self) -> Result<()> {
        self.events
            .retain(|e| !matches!(e, QmpEvent::Reset | QmpEvent::Shutdown));
        self.execute(&qmp::system_reset {})?;
        loop {
            if let Some(index) = self.events.iter().position(|e| *e == QmpEvent::Reset) {
                self.events.remove(index);
                return Ok(());
            }
            if self.events.contains(&QmpEvent::Shutdown) {
                return Err(Error::new(
                    ErrorKind::ConnectionLost,
                    "QEMU shut down instead of resetting the guest (it runs with -no-reboot)",
                ));
            }
            if self.read_message()?.is_none() {
                return Err(lost(&self.endpoint, "the RESET event of system_reset"));
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
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
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

/// Whether a read produced a message: nothing read is the end of the stream, and so is a line cut off before its newline,
/// which is what a QEMU that died in the middle of a write leaves.
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
    let known = match name {
        "STOP" => QmpEvent::Stop,
        "RESUME" => QmpEvent::Resume,
        "RESET" => QmpEvent::Reset,
        "SHUTDOWN" => QmpEvent::Shutdown,
        "POWERDOWN" => QmpEvent::Powerdown,
        other => return Ok(QmpEvent::Other(other.to_owned())),
    };
    serde_json::from_value::<qmp::Event>(value.clone()).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            format!("QEMU's {name} event does not fit the qapi schema ({e}): {value}"),
        )
    })?;
    Ok(known)
}
