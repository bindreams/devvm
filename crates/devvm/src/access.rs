//! Reaching a running guest. U4 fills this in; until then only the type the guest traits mention exists.

/// A connection to a guest's SSH server.
pub(crate) struct SshSession {
    _private: (),
}
