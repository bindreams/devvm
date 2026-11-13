use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use futures::FutureExt;

use super::{Event, run_exec};
use crate::ErrorKind;
use crate::access::{Keepalive, SshSession, VmExit, runtime};
use crate::exit::{ExitStatus, ExitWidth, NullProgress, Report, status_json};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::sshd::Rig;

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
    let running = runtime::spawn(run_exec(session.inner.clone(), "hold".to_owned(), events)).unwrap();
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
