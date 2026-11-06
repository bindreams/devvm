//! The identity of a file, as the OS gives it.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Which file a path named: the volume and the file number. Two paths with equal ids are one file; an id is not
/// unique for ever, since a deleted file's number can be given to a later one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileId {
    /// Windows: the volume serial number. Unix: the device.
    pub volume: u64,
    /// Windows: the 128-bit file id. Unix: the inode.
    pub file: u128,
}

/// How a file looked at by name compares with the file an id was taken from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Comparison {
    /// The same file.
    Same,
    /// Another file: the name leads somewhere else now.
    Different,
    /// No file is at the name.
    Absent,
}

impl FileId {
    /// How `found`, a look at a name (`Host::file_id`), compares with this id. Only a lookup by name can be `Absent`;
    /// the id of an open handle never is. A failure other than a missing file is returned: it says nothing about which
    /// file is there.
    pub(crate) fn compare(&self, found: std::io::Result<FileId>) -> std::io::Result<Comparison> {
        match found {
            Ok(id) if id == *self => Ok(Comparison::Same),
            Ok(_) => Ok(Comparison::Different),
            Err(e) if super::is_absent(&e) => Ok(Comparison::Absent),
            Err(e) => Err(e),
        }
    }
}

/// Text, because JSON has no 128-bit number: `<volume>:<file>` in hex.
impl Serialize for FileId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{:x}:{:x}", self.volume, self.file))
    }
}

impl<'de> Deserialize<'de> for FileId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<FileId, D::Error> {
        let text = String::deserialize(deserializer)?;
        let parsed = text.split_once(':').and_then(|(volume, file)| {
            Some(FileId {
                volume: u64::from_str_radix(volume, 16).ok()?,
                file: u128::from_str_radix(file, 16).ok()?,
            })
        });
        parsed.ok_or_else(|| serde::de::Error::custom(format!("{text:?} is not a file id (`<hex>:<hex>`)")))
    }
}
