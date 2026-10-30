//! Running one command over an SSH channel and mapping how it ended.

use std::sync::Arc;

use russh::{ChannelMsg, Sig};
use tokio::sync::mpsc;

use crate::access::session::Inner;
use crate::exit::ExitStatus;
use crate::{Error, ErrorKind, Result};

/// How `SshSession::exec` ended.
#[derive(Debug)]
pub(crate) enum ExecEnd {
    /// The command ended with this status.
    Finished(ExitStatus),
    /// The host side of the output closed (the error is the failed write or flush). The channel was closed, so the guest
    /// process loses its output pipe and ends by its own SIGPIPE or SIGHUP; devvm does not wait for a status that may
    /// never come, since nothing else would stop a `yes`. The caller decides what that means.
    HostOutputClosed(std::io::Error),
}

/// What the task running the command tells the thread that called `exec`.
pub(super) enum Event {
    Out(Vec<u8>),
    Err(Vec<u8>),
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
    // The channel's end is the authority on whether the connection died, so the guard does not watch the transport.
    let outcome = inner
        .guarded(&awaiting, false, run(&inner, &command, &awaiting, &events, &mut status))
        .await;
    match (outcome, status) {
        (Ok(None), _) => {} // The host side went away; there is nobody to tell.
        (Ok(Some(status)), _) | (Err(_), Some(status)) => {
            let _ = events.send(Event::End(Ok(status))).await;
        }
        (Err(e), None) => {
            let _ = events.send(Event::End(Err(e))).await;
        }
    }
}

/// `Ok(None)` when the host output closed. `status` is set as soon as the command's status arrives.
async fn run(
    inner: &Inner,
    command: &str,
    awaiting: &str,
    events: &mpsc::Sender<Event>,
    status: &mut Option<ExitStatus>,
) -> Result<Option<ExitStatus>> {
    let mut channel = inner
        .handle
        .channel_open_session()
        .await
        .map_err(|_| inner.connection_ended(awaiting))?;
    channel
        .exec(true, command.as_bytes())
        .await
        .map_err(|_| inner.connection_ended(awaiting))?;
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
            ChannelMsg::ExitStatus { exit_status } => {
                *status = Some(ExitStatus::Code(exit_status as i32));
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
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("the server at {} refused to run {awaiting}", inner.ctx.addr),
                ));
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
