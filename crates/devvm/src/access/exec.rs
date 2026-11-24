//! Running one command over an SSH channel and mapping how it ended.

use std::sync::{Arc, Mutex, PoisonError};

use russh::client::Msg;
use russh::{Channel, ChannelMsg, Sig};
use tokio::sync::mpsc;

use crate::access::runtime;
use crate::access::session::{Inner, Watch};
use crate::exit::ExitStatus;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "exec_tests.rs"]
mod exec_tests;

/// How `SshSession::exec` ended.
#[derive(Debug)]
pub(crate) enum ExecEnd {
    /// The command ended with this status.
    Finished(ExitStatus),
    /// The command ended with `status`, but then came what devvm cannot carry (output on a stream it does not read, a
    /// refusal to run the command) or what ended the wait (the VM exited, the boot bound passed); `lost` says what.
    /// The status stands: the error that follows an exit status does not take it back.
    FinishedWithLoss { status: ExitStatus, lost: Error },
    /// The host side of the output closed: a write or flush failed. With no `status`, the command may still be running:
    /// `exec` returns at once without waiting for one, and the task tries to send the channel close, in no order
    /// against the return, until it is sent or fails, the VM exits or the boot bound passes. devvm does not end the
    /// command or wait for it. With a `status`, the command had already ended and nothing is left running.
    HostOutputClosed {
        /// The failed write or flush.
        error: std::io::Error,
        /// The command's status, when it had arrived before the failure.
        status: Option<ExitStatus>,
    },
}

impl ExecEnd {
    /// [`ExecEnd::HostOutputClosed`] for `error`, which also names `lost`, what could not be carried before it.
    pub(super) fn host_output_closed(
        error: std::io::Error,
        status: Option<ExitStatus>,
        lost: Option<Error>,
    ) -> ExecEnd {
        let error = match lost {
            None => error,
            Some(lost) => {
                let text = format!("{error}; the command's output was lost as well ({lost})");
                std::io::Error::new(error.kind(), text)
            }
        };
        ExecEnd::HostOutputClosed { error, status }
    }
}

/// Chunks of guest output the executing task may hold ahead of a slow host writer.
pub(super) const EVENT_BUFFER: usize = 16;

/// What the task running the command tells the thread that called `exec`, through a bounded buffer. Status and losses
/// go through [`Arrived`] so a full buffer cannot hold them back.
pub(super) enum Event {
    Out(Vec<u8>),
    Err(Vec<u8>),
    End(Result<ExitStatus>),
}

/// What the task running the command has seen arrive, published as soon as it did and whatever the buffer holds. `exec`
/// reads it when a write to the host failed, to name what had arrived by then; nothing waits for more.
#[derive(Default)]
pub(super) struct Arrived(Mutex<Seen>);

#[derive(Default)]
struct Seen {
    /// The command's status.
    status: Option<ExitStatus>,
    /// The first thing that happened after the status had arrived and could not be carried; it does not change the
    /// status.
    lost: Option<Error>,
}

impl Arrived {
    fn seen(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn set_status(&self, status: ExitStatus) {
        self.seen().status = Some(status);
    }

    pub(super) fn status(&self) -> Option<ExitStatus> {
        self.seen().status.clone()
    }

    pub(super) fn lose(&self, error: Error) {
        self.seen().lost.get_or_insert(error);
    }

    pub(super) fn lost(&self) -> Option<Error> {
        self.seen().lost.clone()
    }
}

/// Copies the output of the command whose task sends to `events` to `out` and `err`, until the command ends or a write
/// to the host fails.
pub(super) fn relay(
    mut events: mpsc::Receiver<Event>,
    arrived: &Arrived,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> Result<ExecEnd> {
    loop {
        let (to_stderr, chunk) = match runtime::wait(events.recv()) {
            Some(Event::Out(chunk)) => (false, chunk),
            Some(Event::Err(chunk)) => (true, chunk),
            Some(Event::End(result)) => {
                // A partial line the host writers still buffer (a prompt) would otherwise be lost.
                let flushed_out = out.flush();
                let flushed = flushed_out.and(err.flush());
                return match (result, flushed, arrived.lost()) {
                    (Ok(status), Ok(()), None) => Ok(ExecEnd::Finished(status)),
                    (Ok(status), Ok(()), Some(lost)) => Ok(ExecEnd::FinishedWithLoss { status, lost }),
                    (Ok(status), Err(error), lost) => Ok(ExecEnd::host_output_closed(error, Some(status), lost)),
                    (Err(e), Err(error), _) => {
                        Err(e.together_with(format!("flushing the host output failed ({error})")))
                    }
                    (Err(e), Ok(()), _) => Err(e),
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
            // The other sink may hold a partial line (a prompt); its flush is best effort, the write failure is the
            // result.
            let other: &mut dyn std::io::Write = if to_stderr { &mut *out } else { &mut *err };
            let _ = other.flush();
            // Dropping `events` tells the task to close the channel.
            drop(events);
            return Ok(ExecEnd::host_output_closed(e, arrived.status(), arrived.lost()));
        }
    }
}

/// The name an `exit-signal` request carries.
fn signal_name(signal: &Sig) -> String {
    match signal {
        Sig::ABRT => "ABRT".to_owned(),
        Sig::ALRM => "ALRM".to_owned(),
        Sig::FPE => "FPE".to_owned(),
        Sig::HUP => "HUP".to_owned(),
        Sig::ILL => "ILL".to_owned(),
        Sig::INT => "INT".to_owned(),
        Sig::KILL => "KILL".to_owned(),
        Sig::PIPE => "PIPE".to_owned(),
        Sig::QUIT => "QUIT".to_owned(),
        Sig::SEGV => "SEGV".to_owned(),
        Sig::TERM => "TERM".to_owned(),
        Sig::USR1 => "USR1".to_owned(),
        Sig::Custom(name) => name.clone(),
    }
}

/// Runs `command`; sends output and then the end to `events`, and publishes the status and the losses in `arrived`. A
/// receiver that has gone away means the host output closed: the channel is closed and nothing more is sent.
pub(super) async fn run_exec(inner: Arc<Inner>, command: String, events: mpsc::Sender<Event>, arrived: Arc<Arrived>) {
    let awaiting = format!("the command `{command}`");
    // Kept here for the same reason, and for one more: `run` empties it when the channel has ended, so what is left
    // when the guard drops `run` is a channel with a command still running on it.
    let mut channel = None;
    // The channel's end is the authority on whether the connection died, so the guard does not watch the transport.
    let outcome = inner
        .guarded(
            &awaiting,
            Watch::VmAndBoot,
            run(&inner, &command, &awaiting, &events, &arrived, &mut channel),
        )
        .await;
    if let Some(channel) = channel {
        // russh's `Channel` sends no close when dropped: the guest command would keep an output pipe that nobody reads.
        // The close joins the session's bounded queue of outgoing messages, which nothing drains while the peer holds
        // back a window or a socket write (a frozen guest, another channel's upload in flight): the guard that ended
        // `run` bounds this wait too, so the close is sent if there is room and given up if there is none.
        let _ = inner
            .guarded(&awaiting, Watch::VmAndBoot, async { Ok(channel.close().await.is_ok()) })
            .await;
    }
    // Read after `run`, which the guard may have dropped: a status that had arrived is still the command's result.
    match (outcome, arrived.status()) {
        (Ok(None), _) => {} // The host side went away; there is nobody to tell.
        (Ok(Some(status)), _) => {
            let _ = events.send(Event::End(Ok(status))).await;
        }
        // The VM exited or the boot bound passed after the status arrived: the status stands, and the end is said.
        (Err(e), Some(status)) => {
            arrived.lose(e);
            let _ = events.send(Event::End(Ok(status))).await;
        }
        (Err(e), None) => {
            let _ = events.send(Event::End(Err(e))).await;
        }
    }
}

/// `Ok(None)` when the host output closed. `arrived` has the command's status as soon as it arrives. `slot` holds the
/// channel from the moment it is open until `run` returns, so the caller can close it when this future is dropped
/// instead; every other way out has ended the channel already.
async fn run(
    inner: &Inner,
    command: &str,
    awaiting: &str,
    events: &mpsc::Sender<Event>,
    arrived: &Arrived,
    slot: &mut Option<Channel<Msg>>,
) -> Result<Option<ExitStatus>> {
    let opened = inner
        .handle
        .channel_open_session()
        .await
        .map_err(|e| inner.channel_not_opened(awaiting, &e))?;
    let channel = slot.insert(opened);
    let result = pump(inner, channel, command, awaiting, events, arrived).await;
    *slot = None;
    result
}

/// The part of [`run`] on the open `channel`: it closes the channel itself on the ways out that leave the guest command
/// running.
async fn pump(
    inner: &Inner,
    channel: &mut Channel<Msg>,
    command: &str,
    awaiting: &str,
    events: &mpsc::Sender<Event>,
    arrived: &Arrived,
) -> Result<Option<ExitStatus>> {
    channel
        .exec(true, command.as_bytes())
        .await
        .map_err(|_| inner.connection_ended(awaiting))?;
    // The command has no stdin, as with `ssh host cmd </dev/null`: one that reads it would wait forever. A send that
    // fails means the connection is gone, which the wait below reports.
    let _ = channel.eof().await;
    loop {
        let message = tokio::select! {
            message = channel.wait() => message,
            // The host output closed while the guest is quiet: nothing else would wake this task to close the channel.
            () = events.closed() => {
                let _ = channel.close().await;
                return Ok(None);
            }
        };
        let Some(message) = message else {
            // The channel's sender is gone without a close: the connection ended under it. A status that arrived
            // first is still the command's result.
            return arrived
                .status()
                .map(Some)
                .ok_or_else(|| inner.connection_ended(awaiting));
        };
        let event = match message {
            ChannelMsg::Data { data } => Event::Out(data.to_vec()),
            ChannelMsg::ExtendedData { data, ext: 1 } => Event::Err(data.to_vec()),
            // Only stderr is defined; output on another stream would be dropped without a trace.
            ChannelMsg::ExtendedData { ext, .. } => {
                let lost = Error::new(
                    ErrorKind::Io,
                    format!(
                        "the server sent output of {awaiting} on extended data stream {ext}, which devvm does not read"
                    ),
                );
                if arrived.status().is_none() {
                    let _ = channel.close().await;
                    return Err(lost);
                }
                arrived.lose(lost);
                continue;
            }
            ChannelMsg::ExitStatus { exit_status } => {
                // SSH carries the status as 32 unsigned bits; a Windows status such as 0xC0000005 is the same bits as
                // the negative code the guest's own tools report. How many bits a status has is the guest's contract: a
                // Linux server's is 0..=255, and this does not check it.
                arrived.set_status(ExitStatus::Code(exit_status as i32));
                continue;
            }
            ChannelMsg::ExitSignal {
                signal_name: signal,
                core_dumped,
                ..
            } => {
                arrived.set_status(ExitStatus::Signal {
                    name: signal_name(&signal),
                    core_dumped,
                });
                continue;
            }
            ChannelMsg::Failure => {
                let lost = Error::new(
                    ErrorKind::Io,
                    format!("the server at {} refused to run {awaiting}", inner.ctx.addr),
                );
                if arrived.status().is_none() {
                    let _ = channel.close().await;
                    return Err(lost);
                }
                arrived.lose(lost);
                continue;
            }
            ChannelMsg::Close => {
                return arrived.status().map(Some).ok_or_else(|| {
                    Error::new(
                        ErrorKind::NoExitStatus,
                        format!(
                            "the channel for {awaiting} closed without an exit status (serial log: {})",
                            inner.ctx.serial_log.display()
                        ),
                    )
                });
            }
            // Success, Eof and the requests only a client sends.
            _ => continue,
        };
        if events.send(event).await.is_err() {
            let _ = channel.close().await;
            return Ok(None);
        }
    }
}
