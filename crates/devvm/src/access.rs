//! Reaching a running guest. Only the type the guest traits mention exists so far.

/// A connection to a guest's SSH server.
pub(crate) struct SshSession {
    _private: (),
}
