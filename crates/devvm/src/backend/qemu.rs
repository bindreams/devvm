//! The QEMU backend: a pinned sidecar build, driven over QMP.

pub(crate) mod args;
pub(crate) mod backend;
pub(crate) mod machine;
pub(crate) mod ports;
pub(crate) mod qmp;
pub(crate) mod sidecar;
pub(crate) mod storage;
pub(crate) mod uds;
