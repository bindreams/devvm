//! The ports of QEMU's user-mode forwards. A `hostfwd` rule with host port 0 binds an ephemeral port inside QEMU, so
//! the port is read back from HMP `info usernet` once QEMU runs.

use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "ports_tests.rs"]
mod ports_tests;

/// One `hostfwd` rule as it is bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Forward {
    /// The port on `127.0.0.1` that QEMU chose.
    pub host: u16,
    pub guest: u16,
}

/// The host forwards in `info usernet` output, in the order listed. Other rows (open connections, headers) are
/// ignored; a forward row that does not read as `<protocol> <fd> <host address> <host port> <guest address> <guest
/// port> ...` is an error quoting it, and so is a forward bound to an address other than loopback.
pub(crate) fn parse_usernet(text: &str) -> Result<Vec<Forward>> {
    let mut forwards = Vec::new();
    for line in text.lines() {
        let columns: Vec<&str> = line.split_whitespace().collect();
        if !columns
            .first()
            .is_some_and(|protocol| protocol.starts_with("TCP[HOST_FORWARD]"))
        {
            continue;
        }
        let bad = |why: &str| {
            Error::new(
                ErrorKind::Internal,
                format!("cannot read the forwarded port from QEMU's `info usernet` row ({why}): {line:?}"),
            )
        };
        let [_, _, address, host, _, guest, ..] = columns[..] else {
            return Err(bad("too few columns"));
        };
        if address != "127.0.0.1" {
            return Err(bad(&format!("bound to {address}, not 127.0.0.1")));
        }
        let port = |text: &str| text.parse::<u16>().map_err(|_| bad(&format!("{text:?} is not a port")));
        forwards.push(Forward {
            host: port(host)?,
            guest: port(guest)?,
        });
    }
    Ok(forwards)
}
