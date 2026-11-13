# /// script
# requires-python = ">=3.10"
# dependencies = ["pycdlib==1.14.0"]
# ///
"""Writes the small ISOs `iso_tests.rs` reads, next to this file: `uv run make_fixtures.py`.

Each is shaped like a Windows installer ISO (measured on one: UDF only, El Torito entries for BIOS and UEFI whose boot
images are copies kept apart from the UDF `efisys*.bin`, `install.wim` in the UDF tree) with stand-ins for the large
files: 8 KiB `efisys*.bin` images and an `install.wim` that is a 208-byte header and the XML image list of a real ISO
(`wim-*.xml`, UTF-16LE as Windows writes it).
"""

import copy
import io
import struct
from pathlib import Path

import pycdlib

HERE = Path(__file__).parent
IMAGE_SIZE = 8192
HEADER_SIZE = 208

# Windows' own flags (wimlib's RESHDR_FLAG_*): the XML resource of a real install.wim carries METADATA (2).
FLAG_METADATA = 0x02
FLAG_COMPRESSED = 0x04


def image(seed: int, size: int = IMAGE_SIZE) -> bytes:
    return bytes((seed + i * 7) % 251 for i in range(size))


def reshdr(size: int, flags: int, offset: int, original: int) -> bytes:
    return struct.pack("<QQQ", size | flags << 56, offset, original)


def wim(xml_file: str, xml_flags: int = FLAG_METADATA, esd: bool = False, xml_at: int = HEADER_SIZE) -> bytes:
    xml = (HERE / xml_file).read_bytes()
    # ImageTag, cbSize, version, header flags (an ESD says LZMS), chunk size, GUID, part, parts, image count.
    flags = 0x00200002 if esd else 0x00040082
    head = b"MSWIM\0\0\0" + struct.pack("<IIII", HEADER_SIZE, 0x10D00, flags, 32768) + bytes(16)
    head += struct.pack("<HHI", 1, 1, 1)
    head += reshdr(0, 0, 0, 0)  # offset table
    head += reshdr(len(xml), xml_flags, xml_at, len(xml))  # XML data
    head += reshdr(0, 0, 0, 0)  # boot metadata
    head += struct.pack("<I", 0) + reshdr(0, 0, 0, 0)  # boot index, integrity
    head = head.ljust(HEADER_SIZE, b"\0")
    assert len(head) == HEADER_SIZE
    return head.ljust(xml_at, bytes(1)) + xml


def build(name: str, *, xml: str, noprompt_size: int = IMAGE_SIZE, noprompt: bool = True, install: str | None = "install.wim",
          xml_flags: int = FLAG_METADATA, xml_at: int = HEADER_SIZE, extent_types: tuple[int, ...] = (),
          spell=str, also_install: str | None = None) -> None:
    """`extent_types` cuts `install` into extents of 2048 bytes, the last one taking the rest, of these UDF types
    (0 recorded, 1 allocated but not recorded, 2 a hole). `spell` writes every UDF path; `also_install` adds a second
    `install` file under that name."""
    efisys, efisys_noprompt = image(1), image(2, noprompt_size)
    iso = pycdlib.PyCdlib()
    iso.new(interchange_level=3, udf="2.60")

    def add(data: bytes, iso_path: str | None, udf_path: str | None) -> None:
        iso.add_fp(io.BytesIO(data), len(data), iso_path, udf_path=None if udf_path is None else spell(udf_path))

    for udf_dir in ("/efi", "/efi/microsoft", "/efi/microsoft/boot", "/sources"):
        iso.add_directory(udf_path=spell(udf_dir))
    # The UDF files the build reads, and the boot images the firmware loads: copies in other extents.
    add(efisys, None, "/efi/microsoft/boot/efisys.bin")
    if noprompt:
        add(efisys_noprompt, None, "/efi/microsoft/boot/efisys_noprompt.bin")
    if install:
        data = wim(xml, xml_flags, esd=install.endswith(".esd"), xml_at=xml_at)
        add(data, None, f"/sources/{install}")
        if also_install:
            add(data, None, f"/sources/{also_install}")
        if extent_types:
            # A file over 1 GiB is stored in several extents; pycdlib lays them out one after the other.
            entry = iso.get_record(udf_path=spell(f"/sources/{install}"))
            whole, rest = entry.alloc_descs[0], entry.alloc_descs[0].extent_length
            entry.alloc_descs = []
            for index, kind in enumerate(extent_types):
                extent = copy.copy(whole)
                extent.extent_length = rest if index == len(extent_types) - 1 else 2048
                extent.extent_type = kind
                rest -= 2048
                entry.alloc_descs.append(extent)
    add(image(3, 4096), "/ETFSBOOT.BIN;1", None)
    add(efisys, "/EFISYS.BIN;1", None)
    iso.add_eltorito("/ETFSBOOT.BIN;1", "/BOOT.CAT;1", boot_load_size=8)
    iso.add_eltorito("/EFISYS.BIN;1", "/BOOT.CAT;1", efi=True, boot_load_size=1)
    iso.write(HERE / name)
    iso.close()


SINGLE = "wim-win11-eval-en-us.xml"
build("three-images.iso", xml="wim-ltsc-2024-three-images.xml")
build("single-image.iso", xml=SINGLE)
build("boot-images-differ-in-size.iso", xml=SINGLE, noprompt_size=IMAGE_SIZE + 2048)
build("no-no-prompt-image.iso", xml=SINGLE, noprompt=False)
build("install-esd.iso", xml=SINGLE, install="install.esd")
build("install-esd-compressed-xml.iso", xml=SINGLE, install="install.esd", xml_flags=FLAG_COMPRESSED)
build("no-install-image.iso", xml=SINGLE, install=None)
# install.wim in several extents, with its image list in the last.
build("install-wim-in-two-extents.iso", xml=SINGLE, xml_at=4096, extent_types=(0, 0))
build("install-wim-with-an-unwritten-extent.iso", xml=SINGLE, xml_at=4096, extent_types=(0, 1))
build("install-wim-with-a-hole.iso", xml=SINGLE, xml_at=6144, extent_types=(0, 2, 0))
build("install-wim-ending-in-a-hole.iso", xml=SINGLE, extent_types=(0, 2))
# UDF names are exact-case; Windows ISOs differ in case from one another.
build("upper-case-names.iso", xml=SINGLE, spell=str.upper)
build("two-install-names-of-one-spelling.iso", xml=SINGLE, also_install="INSTALL.WIM")
