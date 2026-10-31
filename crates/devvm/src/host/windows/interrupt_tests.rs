use std::io::{Read, Write};

use windows_sys::Win32::Foundation::{ERROR_INVALID_HANDLE, ERROR_NOT_FOUND};

use super::{cancelled_or, interrupt};
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
