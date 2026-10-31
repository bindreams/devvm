//! Running one command over an SSH channel and mapping how it ended.

use std::sync::Arc;

use russh::client::Msg;
use russh::{Channel, ChannelMsg, Sig};
use tokio::sync::mpsc;

use crate::access::session::Inner;
use crate::exit::ExitStatus;
use crate::{Error, ErrorKind, Result};

/// How `SshSession::exec` ended.
#[derive(Debug)]
pub(crate) enum ExecEnd {
    /// The command ended with this status.
    Finished(ExitStatus),
    /// The command ended with `status`, but then came what devvm cannot carry (output on a stream it does not read, a
    /// refusal to run the command) or what ended the wait (the VM exited, the boot bound passed); `lost` says what.
    /// The status stands: the error that follows an exit status does not take it back.
    FinishedWithLoss { status: ExitStatus, lost: Error },
    /// The host side of the output closed. The channel was closed, so the guest process loses its output pipe and
    /// ends by its own SIGPIPE or SIGHUP; devvm does not wait for a status that may never come. The caller decides what
    /// that means.
    HostOutputClosed {
        /// The failed write or flush.
        error: std::io::Error,
        /// The command's status, when it had already ended and only the final flush failed.
        status: Option<ExitStatus>,
    },
}

/// What the task running the command tells the thread that called `exec`.
pub(super) enum Event {
    Out(Vec<u8>),
    Err(Vec<u8>),
    /// Something that happened after the command's status had arrived; it does not change that status.
    Lost(Error),
    End(Result<ExitStatus>),
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

/// Runs `command`; sends output and then the end to `events`. A receiver that has gone away means the host output
/// closed: the channel is closed and nothing more is sent.
pub(super) async fn run_exec(inner: Arc<Inner>, command: String, events: mpsc::Sender<Event>) {
    let awaiting = format!("the command `{command}`");
    // Kept here, not in `run`: the guard drops `run` when the VM exits or the boot bound passes, and a status that had
    // arrived is still the command's result.
    let mut status = None;
    // Kept here for the same reason, and for one more: `run` empties it when the channel has ended, so what is left
    // when the guard drops `run` is a channel with a command still running on it.
    let mut channel = None;
    // The channel's end is the authority on whether the connection died, so the guard does not watch the transport.
    let outcome = inner
        .guarded(
            &awaiting,
            false,
            run(&inner, &command, &awaiting, &events, &mut status, &mut channel),
        )
        .await;
    if let Some(channel) = channel {
        // russh's `Channel` sends no close when dropped: the guest command would keep an output pipe that nobody reads.
        let _ = channel.close().await;
    }
    match (outcome, status) {
        (Ok(None), _) => {} // The host side went away; there is nobody to tell.
        (Ok(Some(status)), _) => {
            let _ = events.send(Event::End(Ok(status))).await;
        }
        // The VM exited or the boot bound passed after the status arrived: the status stands, and the end is said.
        (Err(e), Some(status)) => {
            let _ = events.send(Event::Lost(e)).await;
            let _ = events.send(Event::End(Ok(status))).await;
        }
        (Err(e), None) => {
            let _ = events.send(Event::End(Err(e))).await;
        }
    }
}

/// `Ok(None)` when the host output closed. `status` is set as soon as the command's status arrives. `slot` holds the
/// channel from the moment it is open until `run` returns, so the caller can close it when this future is dropped
/// instead; every other way out has ended the channel already.
async fn run(
    inner: &Inner,
    command: &str,
    awaiting: &str,
    events: &mpsc::Sender<Event>,
    status: &mut Option<ExitStatus>,
    slot: &mut Option<Channel<Msg>>,
) -> Result<Option<ExitStatus>> {
    let opened = inner
        .handle
        .channel_open_session()
        .await
        .map_err(|_| inner.connection_ended(awaiting))?;
    let channel = slot.insert(opened);
    let result = pump(inner, channel, command, awaiting, events, status).await;
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
    status: &mut Option<ExitStatus>,
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
            return status.take().map(Some).ok_or_else(|| inner.connection_ended(awaiting));
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
                if status.is_none() {
                    let _ = channel.close().await;
                    return Err(lost);
                }
                Event::Lost(lost)
            }
            ChannelMsg::ExitStatus { exit_status } => {
                let Ok(code) = i32::try_from(exit_status) else {
                    let _ = channel.close().await;
                    return Err(Error::new(
                        ErrorKind::Io,
                        format!(
                            "the server sent the exit status {exit_status} of {awaiting}, which is over {}",
                            i32::MAX
                        ),
                    ));
                };
                *status = Some(ExitStatus::Code(code));
                continue;
            }
            ChannelMsg::ExitSignal {
                signal_name: signal,
                core_dumped,
                ..
            } => {
                *status = Some(ExitStatus::Signal {
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
                if status.is_none() {
                    let _ = channel.close().await;
                    return Err(lost);
                }
                Event::Lost(lost)
            }
            ChannelMsg::Close => {
                return status.take().map(Some).ok_or_else(|| {
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
