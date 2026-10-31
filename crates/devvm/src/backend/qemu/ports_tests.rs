use super::{Forward, parse_usernet};
use crate::ErrorKind;

/// The text of HMP `info usernet` with one forward, as QEMU 11.1.2 printed it.
const ONE_FORWARD: &str = "\
Hub 0 (#net0):\r
  Protocol[State]    FD  Source Address  Port   Dest. Address  Port RecvQ SendQ\r
  TCP[HOST_FORWARD]  258       127.0.0.1 49986       10.0.2.15    22     0     0\r
";

#[skuld::test]
fn the_measured_info_usernet_text_gives_the_host_and_guest_port() {
    assert_eq!(
        parse_usernet(ONE_FORWARD).unwrap(),
        [Forward { host: 49986, guest: 22 }]
    );
}

#[skuld::test]
fn two_forwards_come_back_in_the_order_listed() {
    let text =
        format!("{ONE_FORWARD}  TCP[HOST_FORWARD]  259       127.0.0.1 49987       10.0.2.15  5985     0     0\r\n");
    let forwards = parse_usernet(&text).unwrap();
    assert_eq!(
        forwards,
        [
            Forward { host: 49986, guest: 22 },
            Forward {
                host: 49987,
                guest: 5985
            }
        ]
    );
}

#[skuld::test]
fn connections_that_are_not_host_forwards_are_ignored() {
    let text = format!(
        "{ONE_FORWARD}  TCP[ESTABLISHED]   260        10.0.2.15 51000     93.184.216.34   443     0     0\r\n  UDP[236 sec]       261        10.0.2.15 50000       10.0.2.3    53     0     0\r\n"
    );
    assert_eq!(parse_usernet(&text).unwrap().len(), 1);
}

#[skuld::test]
fn no_forward_yet_is_an_empty_list() {
    assert_eq!(parse_usernet("").unwrap(), []);
    assert_eq!(
        parse_usernet(
            "Hub 0 (#net0):\r\n  Protocol[State]    FD  Source Address  Port   Dest. Address  Port RecvQ SendQ\r\n"
        )
        .unwrap(),
        []
    );
}

#[skuld::test]
fn a_forward_on_another_address_is_refused() {
    let text = "  TCP[HOST_FORWARD]  258         0.0.0.0 49986       10.0.2.15    22     0     0\r\n";
    let error = parse_usernet(text).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert!(error.message().contains("0.0.0.0"), "{error}");
}

#[skuld::test]
fn an_unparseable_forward_row_is_an_error_quoting_it() {
    let text = "  TCP[HOST_FORWARD]  258       127.0.0.1 notaport       10.0.2.15    22     0     0\r\n";
    let error = parse_usernet(text).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert!(error.message().contains("notaport"), "{error}");
}

#[skuld::test]
fn a_forward_row_with_too_few_columns_is_an_error_quoting_it() {
    let text = "  TCP[HOST_FORWARD]  258       127.0.0.1\r\n";
    let error = parse_usernet(text).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert!(error.message().contains("too few columns"), "{error}");
    assert!(error.message().contains("127.0.0.1"), "{error}");
}

#[skuld::test]
fn a_forward_with_a_guest_port_that_is_not_a_number_is_an_error_quoting_it() {
    let text = "  TCP[HOST_FORWARD]  258       127.0.0.1 49986       10.0.2.15    ssh     0     0\r\n";
    let error = parse_usernet(text).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert!(error.message().contains("ssh"), "{error}");
}
