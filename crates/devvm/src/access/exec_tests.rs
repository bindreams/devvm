use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use futures::FutureExt;

use super::{Arrived, EVENT_BUFFER, Event, ExecEnd, relay, run_exec};
use crate::access::{Keepalive, SshSession, VmExit, runtime};
use crate::exit::{ExitStatus, ExitWidth, Report, status_json};
use crate::progress::NullProgress;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::sshd::Rig;
use crate::{Error, ErrorKind};

/// A keepalive that cannot fire during a test: the stalled server answers nothing, and a connection the keepalive had
/// ended would fail the close at once instead of leaving it waiting.
const NEVER: Keepalive = Keepalive {
    interval: Duration::from_secs(3600),
    max_missed: 1,
};

/// A session whose outgoing queue nothing drains: the server stops reading once it is sent data, and more data than
/// the window it granted is then waiting in the client, which the client's loop does not go on past. Returns the
/// number of messages the queue took until it was full.
fn stall_the_outgoing_queue(session: &SshSession, rig: &Rig) -> (russh::Channel<russh::client::Msg>, usize) {
    let handle = &session.inner.handle;
    let upload = runtime::wait(handle.channel_open_session()).unwrap();
    runtime::wait(upload.exec(true, "stall-on-data")).unwrap();
    // The command is in flight on the server before data follows it: the two travel on different queues of the client.
    rig.sshd.wait_for_commands(2);
    let window = russh::server::Config::default().window_size as usize;
    runtime::wait(handle.data(upload.id(), vec![0u8; window + 1])).unwrap();
    // The server is inside the handler of the first packet, so the client has written the window and keeps the rest,
    // and a client with data it cannot send takes nothing more from its queues.
    rig.sshd.wait_for_held_request();
    // From here on the queue only fills: a send that is not ready now is not ready later.
    let mut taken = 0;
    while upload.eof().now_or_never().is_some() {
        taken += 1;
    }
    (upload, taken)
}

#[skuld::test]
fn a_vm_exit_ends_an_exec_whose_close_has_no_room_in_the_session_queue(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let (fire, wait) = mpsc::channel::<()>();
    let vm = VmExit::when(move || wait.recv().map_err(|e| e.to_string())).unwrap();
    let session = SshSession::connect(&rig.target(), NEVER, vm, None, &NullProgress).unwrap();
    let (events, mut host) = tokio::sync::mpsc::channel(1);
    let running = runtime::spawn(run_exec(
        session.inner.clone(),
        "hold".to_owned(),
        events,
        Arc::default(),
    ))
    .unwrap();
    rig.sshd.wait_for_commands(1);

    let (_upload, taken) = stall_the_outgoing_queue(&session, &rig);
    // The queue holds ten messages; one of them may be the command's own EOF.
    assert!(taken <= 10, "the queue took {taken} messages: something drained it");

    fire.send(()).unwrap();
    // No time bound: a `run_exec` that does not return is a hang, which the CI job's timeout ends.
    let end = runtime::run("the end of run_exec", async move {
        let end = loop {
            match host.recv().await {
                Some(Event::End(result)) => break result,
                Some(_) => {}
                None => panic!("run_exec dropped its events without an end"),
            }
        };
        running.await.expect("run_exec returns");
        end
    })
    .unwrap();
    let error = end.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("VM process exited"), "{error}");
}

/// SSH carries an exit status as 32 unsigned bits, and `ExitStatus::Code` holds the same 32 bits as an `i32`: a
/// Windows status such as `0xC0000005` is the negative code it is on the Windows host.
#[skuld::test]
fn a_wire_status_above_i32_max_is_the_same_32_bits_as_a_negative_code(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let session = rig.connect();
    for (wire, code) in [
        (0xC000_0005u32, -1_073_741_819),
        (0xFFFF_FFFF, -1),
        (0x8000_0000, i32::MIN),
        (0x7FFF_FFFF, i32::MAX),
        (7, 7),
    ] {
        let output = session.exec_collect(&format!("exit {wire}")).unwrap();
        assert_eq!(output.status, ExitStatus::Code(code), "{wire:#x}");
    }
    // The connection is still good.
    assert_eq!(session.exec_collect("exit 0").unwrap().status, ExitStatus::Code(0));
}

#[skuld::test]
fn the_status_file_carries_it_signed(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let status = rig.connect().exec_collect("exit 3221225477").unwrap().status;
    assert_eq!(
        status_json(&Report::Guest(&status)),
        r#"{"guest":{"code":-1073741819}}"#
    );
    assert_eq!(ExitWidth::Full32.exit_code(&status), -1_073_741_819);
}

/// A host sink that refuses every write.
struct Refuses;

impl std::io::Write for Refuses {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "the reader is gone",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An event buffer as the task leaves it when the host is slow: full of output, with the task blocked on the next send.
fn full_of_output() -> tokio::sync::mpsc::Receiver<Event> {
    let (events, buffered) = tokio::sync::mpsc::channel(EVENT_BUFFER);
    for n in 0..EVENT_BUFFER {
        events
            .try_send(Event::Out(
                format!(
                    "{n}
"
                )
                .into_bytes(),
            ))
            .unwrap();
    }
    buffered
}

#[skuld::test]
fn a_failed_write_names_the_status_that_arrived_behind_a_full_event_buffer() {
    let buffered = full_of_output();
    let arrived = Arrived::default();
    arrived.set_status(ExitStatus::Code(7));

    let end = relay(buffered, &arrived, &mut Refuses, &mut Vec::new()).unwrap();

    match end {
        ExecEnd::HostOutputClosed { error, status, .. } => {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
            assert_eq!(status, Some(ExitStatus::Code(7)), "the command's result is not lost");
        }
        other => panic!("{other:?}"),
    }
}

#[skuld::test]
fn a_failed_write_without_a_status_that_arrived_names_none() {
    let end = relay(full_of_output(), &Arrived::default(), &mut Refuses, &mut Vec::new()).unwrap();

    match end {
        ExecEnd::HostOutputClosed { status, .. } => assert_eq!(status, None),
        other => panic!("{other:?}"),
    }
}

#[skuld::test]
fn a_failed_write_names_the_loss_that_arrived_behind_a_full_event_buffer() {
    let arrived = Arrived::default();
    arrived.set_status(ExitStatus::Code(7));
    arrived.lose(Error::new(ErrorKind::ConnectionLost, "the VM process exited"));

    let end = relay(full_of_output(), &arrived, &mut Refuses, &mut Vec::new()).unwrap();

    match end {
        ExecEnd::HostOutputClosed { error, status, .. } => {
            assert_eq!(status, Some(ExitStatus::Code(7)));
            assert!(error.to_string().contains("the reader is gone"), "{error}");
            assert!(error.to_string().contains("the VM process exited"), "{error}");
        }
        other => panic!("{other:?}"),
    }
}

/// A host sink that takes every write, counts flushes and refuses them when told to.
#[derive(Default)]
struct Flushing {
    flushes: usize,
    refuses: bool,
}

impl std::io::Write for Flushing {
    fn write(&mut self, chunk: &[u8]) -> std::io::Result<usize> {
        Ok(chunk.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flushes += 1;
        if self.refuses {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the flush was refused",
            ));
        }
        Ok(())
    }
}

#[skuld::test]
fn a_failed_stderr_write_names_the_status_and_loss_that_arrived_and_flushes_stdout_once() {
    let (events, buffered) = tokio::sync::mpsc::channel(EVENT_BUFFER);
    events.try_send(Event::Err(b"oops".to_vec())).unwrap();
    let arrived = Arrived::default();
    arrived.set_status(ExitStatus::Code(7));
    arrived.lose(Error::new(ErrorKind::ConnectionLost, "the VM process exited"));
    let mut out = Flushing::default();

    let end = relay(buffered, &arrived, &mut out, &mut Refuses).unwrap();

    match end {
        ExecEnd::HostOutputClosed { error, status, .. } => {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
            assert_eq!(status, Some(ExitStatus::Code(7)));
            assert!(error.to_string().contains("the VM process exited"), "{error}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(out.flushes, 1, "the stdout sink may hold a partial line");
}

#[skuld::test]
fn a_failed_flush_at_the_end_names_the_status_and_the_loss_that_arrived() {
    let (events, buffered) = tokio::sync::mpsc::channel(EVENT_BUFFER);
    events.try_send(Event::End(Ok(ExitStatus::Code(3)))).unwrap();
    let arrived = Arrived::default();
    arrived.set_status(ExitStatus::Code(3));
    arrived.lose(Error::new(ErrorKind::ConnectionLost, "the VM process exited"));
    let mut out = Flushing {
        refuses: true,
        ..Flushing::default()
    };

    let end = relay(buffered, &arrived, &mut out, &mut Flushing::default()).unwrap();

    match end {
        ExecEnd::HostOutputClosed { error, status, .. } => {
            assert_eq!(status, Some(ExitStatus::Code(3)));
            assert!(error.to_string().contains("the flush was refused"), "{error}");
            assert!(error.to_string().contains("the VM process exited"), "{error}");
        }
        other => panic!("{other:?}"),
    }
}

#[skuld::test]
fn the_first_loss_stays() {
    let arrived = Arrived::default();
    arrived.lose(Error::new(ErrorKind::Io, "first"));
    arrived.lose(Error::new(ErrorKind::Io, "second"));

    assert_eq!(arrived.lost().expect("a loss").message(), "first");
}

#[skuld::test]
fn a_command_that_ended_with_a_loss_finishes_with_it() {
    let (events, buffered) = tokio::sync::mpsc::channel(EVENT_BUFFER);
    events
        .try_send(Event::Out(
            b"1
"
            .to_vec(),
        ))
        .unwrap();
    events.try_send(Event::End(Ok(ExitStatus::Code(7)))).unwrap();
    let arrived = Arrived::default();
    arrived.set_status(ExitStatus::Code(7));
    arrived.lose(Error::new(ErrorKind::ConnectionLost, "the VM process exited"));
    let mut out = Vec::new();

    let end = relay(buffered, &arrived, &mut out, &mut Vec::new()).unwrap();

    assert_eq!(
        out,
        b"1
"
    );
    match end {
        ExecEnd::FinishedWithLoss { status, lost } => {
            assert_eq!(status, ExitStatus::Code(7));
            assert!(lost.message().contains("VM process exited"), "{lost}");
        }
        other => panic!("{other:?}"),
    }
}

#[skuld::test]
fn a_closed_host_output_names_what_was_lost_before_it() {
    let lost = Error::new(ErrorKind::ConnectionLost, "the VM process exited").with_serial_log("/x/serial.log");
    let broken = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the reader is gone");
    match ExecEnd::host_output_closed(broken, Some(ExitStatus::Code(7)), Some(lost)) {
        ExecEnd::HostOutputClosed {
            error,
            status,
            serial_log,
        } => {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
            assert_eq!(status, Some(ExitStatus::Code(7)));
            assert_eq!(serial_log.as_deref(), Some(std::path::Path::new("/x/serial.log")));
            let text = error.to_string();
            assert!(text.contains("the reader is gone"), "{text}");
            assert!(
                text.contains("the command's output was lost as well (connection-lost: the VM process exited)"),
                "{text}"
            );
        }
        other => panic!("{other:?}"),
    }
}
