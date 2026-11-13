//! The image list of a Windows installation file (`install.wim`, `install.esd`): the XML a WIM keeps after its header.
//!
//! The header is read here (about 20 lines) and the XML by `quick-xml`. Only an XML resource stored uncompressed is
//! read, which is how Windows writes it; a list stored compressed is refused by name.

use std::io::{Read, Seek, SeekFrom};

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "wim_tests.rs"]
mod wim_tests;

const TAG: &[u8; 8] = b"MSWIM\0\0\0";
/// A WIM written for a pipe; not a file Setup installs from.
const PIPABLE_TAG: &[u8; 8] = b"WLPWM\0\0\0";
const HEADER_SIZE: u64 = 208;
/// Where the XML data's `RESHDR_DISK_SHORT` (size and flags, offset, uncompressed size) sits in the header.
const XML_ENTRY: usize = 72;
/// A resource's flag byte: the resource is stored compressed.
const RESOURCE_COMPRESSED: u8 = 0x04;

/// The architecture an image is for, as its `ARCH` element says (the `PROCESSOR_ARCHITECTURE_*` values).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WimArch {
    X86,
    Arm,
    Ia64,
    X64,
    Arm64,
    Other(u32),
}

impl WimArch {
    pub(crate) fn from_code(code: u32) -> WimArch {
        match code {
            0 => WimArch::X86,
            5 => WimArch::Arm,
            6 => WimArch::Ia64,
            9 => WimArch::X64,
            12 => WimArch::Arm64,
            other => WimArch::Other(other),
        }
    }

    /// The name and the code, as an error says it: `arm64 (ARCH 12)`.
    pub(crate) fn describe(self) -> String {
        let (name, code) = match self {
            WimArch::X86 => ("x86", 0),
            WimArch::Arm => ("arm", 5),
            WimArch::Ia64 => ("ia64", 6),
            WimArch::X64 => ("x64", 9),
            WimArch::Arm64 => ("arm64", 12),
            WimArch::Other(code) => return format!("an unknown architecture (ARCH {code})"),
        };
        format!("{name} (ARCH {code})")
    }
}

/// One image of an installation file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WimImage {
    /// The image's number in the file, from 1.
    pub index: u32,
    /// `NAME`: what the recipe's `edition` says.
    pub name: String,
    /// `EDITIONID`, such as `EnterpriseS`; empty when the image has none.
    pub edition_id: String,
    pub arch: WimArch,
    /// The image's default language, such as `en-US`.
    pub language: String,
}

impl WimImage {
    /// Whether the image is an evaluation edition: its edition id ends in `Eval`, in any case.
    pub(crate) fn is_evaluation(&self) -> bool {
        let id = self.edition_id.as_bytes();
        id.len() >= 4 && id[id.len() - 4..].eq_ignore_ascii_case(b"eval")
    }
}

/// The images of the WIM `file` (its name, for errors) that `reader` reads.
pub(crate) fn read_image_list(reader: &mut (impl Read + Seek), file: &str) -> Result<Vec<WimImage>> {
    let io = |what: &str, e: std::io::Error| Error::io(ErrorKind::Io, format!("cannot read {what} of {file}"), e);
    let length = reader.seek(SeekFrom::End(0)).map_err(|e| io("the size", e))?;
    reader.seek(SeekFrom::Start(0)).map_err(|e| io("the header", e))?;
    let mut header = [0u8; HEADER_SIZE as usize];
    if length < HEADER_SIZE {
        return Err(not_a_wim(file, "it is shorter than a WIM header"));
    }
    reader.read_exact(&mut header).map_err(|e| io("the header", e))?;
    if header[..8] == *PIPABLE_TAG {
        return Err(not_a_wim(file, "it is a pipable WIM"));
    }
    if header[..8] != *TAG {
        return Err(not_a_wim(file, "it does not start with the WIM tag"));
    }
    let size_and_flags = u64::from_le_bytes(header[XML_ENTRY..XML_ENTRY + 8].try_into().expect("8 bytes"));
    let (size, flags) = (size_and_flags & ((1 << 56) - 1), (size_and_flags >> 56) as u8);
    let offset = u64::from_le_bytes(header[XML_ENTRY + 8..XML_ENTRY + 16].try_into().expect("8 bytes"));
    if flags & RESOURCE_COMPRESSED != 0 {
        return Err(Error::new(
            ErrorKind::Recipe,
            format!(
                "{file} keeps its image list compressed (the compressed flag of its XML resource is set), which devvm cannot read"
            ),
        ));
    }
    if offset.checked_add(size).is_none_or(|end| end > length) || size % 2 != 0 {
        return Err(not_a_wim(file, "its XML resource lies outside the file"));
    }
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|e| io("the image list", e))?;
    let mut bytes = vec![0u8; size as usize];
    reader.read_exact(&mut bytes).map_err(|e| io("the image list", e))?;
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    let xml = String::from_utf16(&units).map_err(|_| not_a_wim(file, "its image list is not UTF-16"))?;
    parse_image_list(xml.strip_prefix('\u{feff}').unwrap_or(&xml), file)
}

fn not_a_wim(file: &str, why: &str) -> Error {
    Error::new(ErrorKind::Recipe, format!("{file} is not a Windows image file: {why}"))
}

/// The images `xml`, the text of a WIM's image list, describes.
fn parse_image_list(xml: &str, file: &str) -> Result<Vec<WimImage>> {
    let bad = |why: String| {
        Error::new(
            ErrorKind::Recipe,
            format!("the image list of {file} is unreadable: {why}"),
        )
    };
    let mut reader = Reader::from_str(xml);
    let mut path: Vec<String> = Vec::new();
    let mut images: Vec<Partial> = Vec::new();
    loop {
        match reader.read_event().map_err(|e| bad(e.to_string()))? {
            Event::Start(start) => {
                let name = start.name().as_ref().to_owned();
                if path == ["WIM"] && name == "IMAGE" {
                    let index = start
                        .attributes()
                        .filter_map(std::result::Result::ok)
                        .find(|attribute| attribute.key.as_ref() == "INDEX")
                        .and_then(|attribute| attribute.value.trim().parse::<u32>().ok())
                        .ok_or_else(|| bad("an IMAGE has no numeric INDEX".to_owned()))?;
                    images.push(Partial::new(index));
                }
                path.push(name);
            }
            Event::End(_) => {
                path.pop();
            }
            Event::Text(text) => {
                let text = text.xml10_content();
                add_text(&mut images, &path, &text);
            }
            Event::GeneralRef(reference) => {
                let name = reference.xml10_content();
                let resolved = match reference.resolve_char_ref().map_err(|e| bad(e.to_string()))? {
                    Some(c) => c.to_string(),
                    None => match name.as_ref() {
                        "amp" => "&".to_owned(),
                        "lt" => "<".to_owned(),
                        "gt" => ">".to_owned(),
                        "quot" => "\"".to_owned(),
                        "apos" => "'".to_owned(),
                        other => return Err(bad(format!("the entity &{other}; is not defined"))),
                    },
                };
                add_text(&mut images, &path, &resolved);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    images.into_iter().map(|image| image.finish(file)).collect()
}

/// An image while its element is being read.
struct Partial {
    index: u32,
    name: Option<String>,
    edition_id: Option<String>,
    arch: Option<String>,
    language: Option<String>,
}

impl Partial {
    fn new(index: u32) -> Partial {
        Partial {
            index,
            name: None,
            edition_id: None,
            arch: None,
            language: None,
        }
    }

    fn finish(self, file: &str) -> Result<WimImage> {
        let missing = |what: &str| {
            Error::new(
                ErrorKind::Recipe,
                format!("image {} of {file} has no {what}", self.index),
            )
        };
        let name = self
            .name
            .clone()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| missing("NAME"))?;
        let code = self
            .arch
            .as_deref()
            .ok_or_else(|| missing("ARCH"))?
            .trim()
            .parse::<u32>()
            .map_err(|_| missing("numeric ARCH"))?;
        let language = self
            .language
            .clone()
            .filter(|language| !language.is_empty())
            .ok_or_else(|| missing("default language"))?;
        Ok(WimImage {
            index: self.index,
            name,
            edition_id: self.edition_id.unwrap_or_default(),
            arch: WimArch::from_code(code),
            language,
        })
    }
}

/// Appends `text`, found at `path`, to the field of the image being read that the path names.
fn add_text(images: &mut [Partial], path: &[String], text: &str) {
    let Some(image) = images.last_mut() else { return };
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    let field = match path.as_slice() {
        ["WIM", "IMAGE", "NAME"] => &mut image.name,
        ["WIM", "IMAGE", "WINDOWS", "EDITIONID"] => &mut image.edition_id,
        ["WIM", "IMAGE", "WINDOWS", "ARCH"] => &mut image.arch,
        ["WIM", "IMAGE", "WINDOWS", "LANGUAGES", "DEFAULT"] => &mut image.language,
        _ => return,
    };
    field.get_or_insert_with(String::new).push_str(text);
}
