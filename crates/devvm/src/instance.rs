//! Instances: named VMs made from an image, tracked by files and OS locks under the state directory.

pub(crate) mod dir;
pub mod info;
pub(crate) mod locks;
pub(crate) mod name;
pub(crate) mod reach;
pub(crate) mod reaper;
pub(crate) mod record;
pub(crate) mod registry;
