use std::io::{Cursor, Read, Seek, SeekFrom};

use super::{WimArch, WimImage, parse_image_list, read_image_list};
use crate::ErrorKind;
use crate::test_support::fixtures::iso_fixture;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(iso_fixture(name)).unwrap()
}

/// A WIM that is a 208-byte header and `xml`; `flags` is the flag byte of the XML resource.
pub(crate) fn wim_with(xml: &[u8], flags: u8) -> Vec<u8> {
    let mut header = vec![0u8; 208];
    header[..8].copy_from_slice(b"MSWIM\0\0\0");
    header[8..12].copy_from_slice(&208u32.to_le_bytes());
    header[72..80].copy_from_slice(&((xml.len() as u64) | u64::from(flags) << 56).to_le_bytes());
    header[80..88].copy_from_slice(&208u64.to_le_bytes());
    header[88..96].copy_from_slice(&(xml.len() as u64).to_le_bytes());
    [header, xml.to_vec()].concat()
}

fn utf16(text: &str) -> Vec<u8> {
    std::iter::once(0xfeffu16)
        .chain(text.encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn images(fixture_name: &str) -> Vec<WimImage> {
    read_image_list(&mut Cursor::new(wim_with(&fixture(fixture_name), 2)), "install.wim").unwrap()
}

fn image(index: u32, name: &str, edition_id: &str, arch: WimArch, language: &str) -> WimImage {
    WimImage {
        index,
        name: name.to_owned(),
        edition_id: edition_id.to_owned(),
        arch,
        language: language.to_owned(),
    }
}

#[skuld::test]
fn the_three_images_of_an_ltsc_iso_are_listed() {
    assert_eq!(
        images("wim-ltsc-2024-three-images.xml"),
        [
            image(
                1,
                "Windows 11 Enterprise LTSC 2024",
                "EnterpriseS",
                WimArch::X64,
                "en-US"
            ),
            image(
                2,
                "Windows 11 IoT Enterprise LTSC 2024",
                "IoTEnterpriseS",
                WimArch::X64,
                "en-US"
            ),
            image(
                3,
                "Windows 11 IoT Enterprise Subscription LTSC 2024",
                "IoTEnterpriseSK",
                WimArch::X64,
                "en-US"
            ),
        ]
    );
}

#[skuld::test]
fn the_install_language_is_the_images_default_language() {
    assert_eq!(images("wim-win11-eval-en-us.xml")[0].language, "en-US");
    // The de-DE image lists en-US as a fallback language of its own; the default is the one that counts.
    assert_eq!(images("wim-win11-eval-de-de.xml")[0].language, "de-DE");
}

#[skuld::test]
fn the_ltsc_evaluation_image_is_listed() {
    assert_eq!(
        images("wim-ltsc-2024-eval.xml"),
        [image(
            1,
            "Windows 11 IoT Enterprise LTSC 2024 Evaluation",
            "IoTEnterpriseSEval",
            WimArch::X64,
            "en-US"
        )]
    );
}

#[skuld::test]
fn an_edition_id_ending_in_eval_is_an_evaluation_image() {
    let with = |id: &str| image(1, "n", id, WimArch::X64, "en-US").is_evaluation();
    assert!(with("EnterpriseEval"));
    assert!(with("IoTEnterpriseSEval"));
    assert!(with("professionaleval"));
    assert!(!with("EnterpriseS"));
    assert!(!with("IoTEnterpriseS"));
    assert!(!with(""));
    assert!(!with("val"));
}

#[skuld::test]
fn an_image_list_in_a_compressed_xml_resource_is_refused_naming_the_file() {
    let wim = wim_with(&fixture("wim-win11-eval-en-us.xml"), 0x04);
    let error = read_image_list(&mut Cursor::new(wim), "install.esd").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("install.esd"), "{error}");
    assert!(error.message().contains("compressed"), "{error}");
}

#[skuld::test]
fn a_file_without_the_wim_tag_is_refused() {
    let error = read_image_list(&mut Cursor::new(vec![7u8; 300]), "install.wim").unwrap_err();
    assert!(
        error.message().contains("install.wim is not a Windows image file"),
        "{error}"
    );
    let error = read_image_list(&mut Cursor::new(vec![7u8; 20]), "install.wim").unwrap_err();
    assert!(error.message().contains("shorter than a WIM header"), "{error}");
}

#[skuld::test]
fn an_xml_resource_past_the_end_of_the_file_is_refused_without_reading_it() {
    let mut wim = wim_with(b"ab", 2);
    wim[72..80].copy_from_slice(&(1u64 << 40).to_le_bytes());
    let error = read_image_list(&mut Cursor::new(wim), "install.wim").unwrap_err();
    assert!(error.message().contains("outside the file"), "{error}");
}

#[skuld::test]
fn an_odd_xml_resource_size_is_its_own_error() {
    let mut wim = wim_with(b"abc", 2);
    let error = read_image_list(&mut Cursor::new(wim.clone()), "install.wim").unwrap_err();
    assert!(error.message().contains("odd"), "{error}");
    assert!(!error.message().contains("outside the file"), "{error}");
    // The size is checked against the file before the XML is read.
    wim[72..80].copy_from_slice(&((1u64 << 40) | 1).to_le_bytes());
    let error = read_image_list(&mut Cursor::new(wim), "install.wim").unwrap_err();
    assert!(error.message().contains("outside the file"), "{error}");
}

/// A file of `length` bytes that starts with `head` and is zeros after it, which counts what is read from it.
struct Sparse {
    head: Vec<u8>,
    length: u64,
    position: u64,
    served: u64,
}

impl Read for Sparse {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let count = (self.length - self.position).min(buf.len() as u64) as usize;
        for (i, byte) in buf[..count].iter_mut().enumerate() {
            *byte = self.head.get((self.position + i as u64) as usize).copied().unwrap_or(0);
        }
        self.position += count as u64;
        self.served += count as u64;
        Ok(count)
    }
}

impl Seek for Sparse {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.position = match to {
            SeekFrom::Start(offset) => offset,
            SeekFrom::End(back) => self.length.checked_add_signed(back).unwrap(),
            SeekFrom::Current(by) => self.position.checked_add_signed(by).unwrap(),
        };
        Ok(self.position)
    }
}

#[skuld::test]
fn the_image_list_is_read_as_far_as_it_goes_not_as_far_as_the_header_claims() {
    // The header claims 4 GiB of XML in an 8 GiB file; the list itself ends after a few hundred bytes.
    let xml = utf16(
        "<WIM><IMAGE INDEX=\"1\"><NAME>x</NAME><WINDOWS><ARCH>9</ARCH><LANGUAGES><DEFAULT>en-US</DEFAULT></LANGUAGES>\
         </WINDOWS></IMAGE></WIM>",
    );
    let mut head = wim_with(&xml, 2);
    head[72..80].copy_from_slice(&(4u64 << 30).to_le_bytes());
    let mut file = Sparse {
        head,
        length: 8 << 30,
        position: 0,
        served: 0,
    };
    let list = read_image_list(&mut file, "install.wim").unwrap();
    assert_eq!(list[0].name, "x");
    assert!(file.served < 1 << 20, "{} bytes were read", file.served);
}

#[skuld::test]
fn text_across_the_decoders_chunks_is_read_whole_and_a_broken_pair_is_refused() {
    // Enough surrogate pairs to cross any chunk size, so a pair is split by one.
    let name = "\u{1f600}".repeat(20_000);
    let xml = format!(
        "<WIM><IMAGE INDEX=\"1\"><NAME>{name}</NAME><WINDOWS><ARCH>9</ARCH><LANGUAGES><DEFAULT>en-US</DEFAULT>\
         </LANGUAGES></WINDOWS></IMAGE></WIM>"
    );
    let list = read_image_list(&mut Cursor::new(wim_with(&utf16(&xml), 2)), "install.wim").unwrap();
    assert_eq!(list[0].name, name);

    // A lone high surrogate where the text ends.
    let mut broken = utf16("<WIM><IMAGE INDEX=\"1\"><NAME>a");
    broken.extend_from_slice(&0xd83du16.to_le_bytes());
    broken.extend(utf16("</NAME></IMAGE></WIM>").iter().skip(2));
    let error = read_image_list(&mut Cursor::new(wim_with(&broken, 2)), "install.wim").unwrap_err();
    assert!(error.message().contains("not UTF-16"), "{error}");
}

#[skuld::test]
fn a_name_with_an_entity_is_read_whole() {
    let xml = "<WIM><IMAGE INDEX=\"2\"><WINDOWS><ARCH>9</ARCH><LANGUAGES><DEFAULT>de-DE</DEFAULT></LANGUAGES></WINDOWS>\
               <NAME>Pro &amp; Home &#228;</NAME></IMAGE></WIM>";
    let list = read_image_list(&mut Cursor::new(wim_with(&utf16(xml), 2)), "install.wim").unwrap();
    assert_eq!(list[0].name, "Pro & Home \u{e4}");
    assert_eq!(list[0].index, 2);
    assert_eq!(list[0].edition_id, "");
}

#[skuld::test]
fn an_image_without_a_default_language_or_a_name_is_refused_naming_it() {
    let no_language = "<WIM><IMAGE INDEX=\"1\"><NAME>x</NAME><WINDOWS><ARCH>9</ARCH></WINDOWS></IMAGE></WIM>";
    let error = parse_image_list(no_language.as_bytes(), "install.wim").unwrap_err();
    assert!(
        error
            .message()
            .contains("image 1 of install.wim has no default language"),
        "{error}"
    );
    let no_name = "<WIM><IMAGE INDEX=\"3\"><WINDOWS><ARCH>9</ARCH></WINDOWS></IMAGE></WIM>";
    let error = parse_image_list(no_name.as_bytes(), "install.wim").unwrap_err();
    assert!(
        error.message().contains("image 3 of install.wim has no NAME"),
        "{error}"
    );
}

#[skuld::test]
fn the_architecture_codes_are_named() {
    assert_eq!(WimArch::from_code(9), WimArch::X64);
    assert_eq!(WimArch::from_code(12).describe(), "arm64 (ARCH 12)");
    assert_eq!(WimArch::from_code(77).describe(), "an unknown architecture (ARCH 77)");
}

#[skuld::test]
fn an_image_list_that_ends_inside_an_element_is_refused() {
    let cut = "<WIM><IMAGE INDEX=\"1\"><NAME>x</NAME><WINDOWS><ARCH>9</ARCH><LANGUAGES><DEFAULT>en-US</DEFAULT>";
    let error = parse_image_list(cut.as_bytes(), "install.wim").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(
        error.message().contains("install.wim") && error.message().contains("ends"),
        "{error}"
    );
}

#[skuld::test]
fn an_image_written_as_an_empty_element_is_refused_naming_it() {
    let list = "<WIM><IMAGE INDEX=\"2\"/></WIM>";
    let error = parse_image_list(list.as_bytes(), "install.wim").unwrap_err();
    assert!(
        error.message().contains("image 2 of install.wim has no NAME"),
        "{error}"
    );
}

#[skuld::test]
fn a_field_written_as_an_empty_element_is_missing() {
    let list = "<WIM><IMAGE INDEX=\"1\"><NAME/><WINDOWS><ARCH>9</ARCH><LANGUAGES><DEFAULT>en-US</DEFAULT></LANGUAGES>\
                </WINDOWS></IMAGE></WIM>";
    let error = parse_image_list(list.as_bytes(), "install.wim").unwrap_err();
    assert!(
        error.message().contains("image 1 of install.wim has no NAME"),
        "{error}"
    );
}
