//! The downloads of a Windows base, each pinned by content: the base is a pure function of its pins and templates.
//!
//! The virtio-win ISO is fetched by the host. The three zips are downloaded by the first-logon script in the guest,
//! which checks each sha256 before it extracts anything.

use crate::store::fetch::Artifact;

#[cfg(test)]
#[path = "pins_tests.rs"]
mod pins_tests;

/// One pinned download.
pub(super) struct Pin {
    /// What it is, in words an error message can use.
    pub what: &'static str,
    pub url: &'static str,
    /// 64 lower-case hex digits.
    pub sha256: &'static str,
}

impl Pin {
    /// The pin as the host's verified downloader takes it.
    pub(super) fn artifact(&self) -> Artifact {
        Artifact {
            url: self.url.to_owned(),
            sha256: self
                .sha256
                .parse()
                .expect("every pin is 64 hex digits (checked by a test)"),
        }
    }
}

/// The storage and network drivers the installer and the installed system load.
pub(super) const VIRTIO_WIN: Pin = Pin {
    what: "virtio-win 0.1.302",
    url: "https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/archive-virtio/virtio-win-0.1.302-1/virtio-win-0.1.302.iso",
    sha256: "303f7ae40dad495d6ae474fdc571df58958a4dbc5c37a522d80f9a203867949d",
};

/// Win32-OpenSSH. No build of ours (owner, 2026-11-13); the sshd pin is a Preview release because no other is current.
pub(super) const OPENSSH: Pin = Pin {
    what: "Win32-OpenSSH 10.4.0.0p1-Preview",
    url: "https://github.com/PowerShell/Win32-OpenSSH/releases/download/10.4.0.0p1-Preview/OpenSSH-Win64.zip",
    sha256: "b67ff8becd203502190ec42ac7787c24c2cd3325ed7700485fe7a1c83d2a7957",
};

pub(super) const POWERSHELL: Pin = Pin {
    what: "PowerShell 7.7.0",
    url: "https://github.com/PowerShell/PowerShell/releases/download/v7.7.0/PowerShell-7.7.0-win-x64.zip",
    sha256: "7c74e03b20a75b59715847439d046a6214b1863ee79b187872980f139bb4a05f",
};

pub(super) const SSH_BROKER: Pin = Pin {
    what: "ssh-broker 0.1.0",
    url: "https://github.com/bindreams/ssh-broker/releases/download/v0.1.0/ssh-broker-0.1.0-x86_64-pc-windows-msvc.zip",
    sha256: "b23a5556eca2cea6bfbd7dbd48dd98aebf557d5867e7b6e00961ada7fb24bf3b",
};

/// Every pin, in the order the definition hash reads them.
pub(super) const ALL: &[&Pin] = &[&VIRTIO_WIN, &OPENSSH, &POWERSHELL, &SSH_BROKER];
