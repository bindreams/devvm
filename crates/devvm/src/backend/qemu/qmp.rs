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
use crate::exit::Progress;
use crate::host::{Link, ProcessWatch};
use crate::instance::record::InstanceId;
use crate::store::lock::{FileLock, Mode};
use crate::{Arch, Error, ErrorKind, Result};

#[cfg(test)]
#[path = "qmp_tests.rs"]
pub(crate) mod qmp_tests;

#[cfg(all(test, windows))]
#[path = "qmp_windows_tests.rs"]
mod qmp_windows_tests;

// Endpoint ============================================================================================================

/// Where a machine's QMP socket is, and the lock file beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QmpEndpoint {
    path: PathBuf,
}

impl QmpEndpoint {
    /// `<temp>/dvm-<32 hex of the id>-<8 random hex>.sock`: short, so it fits `sun_path` under any temp directory, and
    /// unique per launch. A path is never reused, so deleting a dead machine's files by path cannot touch a later
    /// machine's. A path over `path_max` bytes (`Host::socket_path_max`) is refused here, before a machine is launched
    /// with it.
    pub(crate) fn for_launch(temp: &Path, id: &InstanceId, path_max: usize) -> Result<QmpEndpoint> {
        let mut launch = [0u8; 4];
        getrandom::fill(&mut launch).expect("the OS random number generator is available");
        let path = temp.join(format!("dvm-{id}-{}.sock", hex::encode(launch)));
        let bytes = path.to_string_lossy().len();
        if bytes > path_max {
            return Err(Error::new(
                ErrorKind::Unavailable,
                format!(
                    "the QEMU control socket path {} is {bytes} bytes, over the {path_max}-byte limit of an AF_UNIX \
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
    /// process that has exited fails before a lock file is touched.
    ///
    /// Never deletes the lock file, whether it succeeds or not: another session may be waiting on that very file, and a
    /// path removed under it could be a later session's. The file is removed by whoever knows no session can be live,
    /// the machine's shutdown or kill. Each launch has its own path, so a deletion by path only ever touches that
    /// machine's files.
    ///
    /// A read on the session ends once the process has exited and QEMU's bytes are used up. On Windows the exit ends
    /// the read even if the OS never delivers the end of the stream; elsewhere the read ends with the stream, so a
    /// process that keeps the socket open after QEMU exits keeps the read waiting.
    pub(crate) fn connect(ep: &QmpEndpoint, process: &Arc<dyn ProcessWatch>, progress: &dyn Progress) -> Result<Qmp> {
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
