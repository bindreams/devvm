use std::io::{Read, Write};

use windows_sys::Win32::Foundation::{ERROR_INVALID_HANDLE, ERROR_NOT_FOUND};

use super::{cancelled_or, interrupt, interrupt_with};
use crate::test_support::qemu::Sock;

#[skuld::test]
fn nothing_pending_is_not_a_failure() {
    assert!(cancelled_or(true, || unreachable!("success has no error code")).is_ok());
    assert!(cancelled_or(false, || ERROR_NOT_FOUND).is_ok());
}

#[skuld::test]
fn any_other_cancel_failure_is_reported() {
    let error = cancelled_or(false, || ERROR_INVALID_HANDLE).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(ERROR_INVALID_HANDLE as i32));
}

#[skuld::test]
fn a_cancel_that_worked_needs_no_fallback() {
    assert!(interrupt_with(Ok(()), || unreachable!("the cancel worked")).is_ok());
}

#[skuld::test]
fn a_cancel_that_failed_shuts_the_connection_down_and_reports_the_failure() {
    let mut shut_down = 0;
    let error = interrupt_with(Err(std::io::Error::other("no cancel for you")), || {
        shut_down += 1;
        Ok(())
    })
    .unwrap_err();

    assert_eq!(shut_down, 1);
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    let text = error.to_string();
    assert!(
        text.contains("no cancel for you") && text.contains("shut down instead"),
        "{text}"
    );
}

#[skuld::test]
fn a_failed_fallback_is_reported_with_the_failed_cancel() {
    let error = interrupt_with(Err(std::io::Error::other("no cancel for you")), || {
        Err(std::io::Error::other("no shutdown either"))
    })
    .unwrap_err();

    let text = error.to_string();
    assert!(
        text.contains("no cancel for you") && text.contains("no shutdown either"),
        "{text}"
    );
}

#[skuld::test]
fn what_the_peer_wrote_is_still_read_after_the_interrupt_and_the_next_read_does_not_block() {
    let sock = Sock::new();
    let listener = sock.listen();
    let mut client = crate::backend::qemu::uds::UnixStream::connect(sock.endpoint.path()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.write_all(b"hello\n").unwrap();

    interrupt(&client.try_clone().unwrap()).unwrap();

    let mut buffer = [0u8; 16];
    let read = client.read(&mut buffer).unwrap();
    assert_eq!(&buffer[..read], b"hello\n");
    // The peer still holds its end open: a blocking read would never return.
    let error = client.read(&mut buffer).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock, "{error}");
}
