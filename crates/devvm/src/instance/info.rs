//! What `ls` shows about an instance.

use std::path::PathBuf;
use std::time::SystemTime;

use serde::Serialize;

use crate::{Arch, GuestKind};

/// Whether an instance's VM is running, judged by the recorded process identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstanceState {
    Running,
    Stopped,
    /// The OS refused to say whether the recorded process is alive.
    Unknown,
}

impl InstanceState {
    pub fn as_str(self) -> &'static str {
        match self {
            InstanceState::Running => "running",
            InstanceState::Stopped => "stopped",
            InstanceState::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceInfo {
    pub name: String,
    pub state: InstanceState,
    pub guest: GuestKind,
    pub arch: Arch,
    pub created: SystemTime,
    /// The directory the instance was created from.
    pub created_from: PathBuf,
    pub expires: Option<SystemTime>,
    /// `true` once the instance stays until `rm`; `false` while a devvm process owns it.
    pub ownerless: bool,
}

/// The keys of one `ls --json` line, in documented order.
#[derive(Serialize)]
struct Line<'a> {
    name: &'a str,
    state: &'a str,
    guest: &'a str,
    arch: &'a str,
    created: String,
    created_from: String,
    expires: Option<String>,
    owner: &'a str,
}

fn time_text(time: SystemTime) -> String {
    jiff::Timestamp::try_from(time).map_or_else(|_| "out-of-range".to_owned(), |t| t.to_string())
}

impl InstanceInfo {
    /// One JSON object on one line, without a trailing newline. Keys: `name`, `state`, `guest`, `arch`, `created`,
    /// `created_from`, `expires` (`null` when none), `owner` (`"process"` or `"none"`).
    pub fn to_json_line(&self) -> String {
        let line = Line {
            name: &self.name,
            state: self.state.as_str(),
            guest: self.guest.as_str(),
            arch: self.arch.as_str(),
            created: time_text(self.created),
            created_from: self.created_from.to_string_lossy().into_owned(),
            expires: self.expires.map(time_text),
            owner: if self.ownerless { "none" } else { "process" },
        };
        serde_json::to_string(&line).expect("a struct of strings serializes")
    }
}
