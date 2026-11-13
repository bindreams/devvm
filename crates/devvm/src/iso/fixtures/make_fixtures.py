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


def wim(xml_file: str, xml_flags: int = FLAG_METADATA, esd: bool = False) -> bytes:
    xml = (HERE / xml_file).read_bytes()
    # ImageTag, cbSize, version, header flags (an ESD says LZMS), chunk size, GUID, part, parts, image count.
    flags = 0x00200002 if esd else 0x00040082
    head = b"MSWIM\0\0\0" + struct.pack("<IIII", HEADER_SIZE, 0x10D00, flags, 32768) + bytes(16)
    head += struct.pack("<HHI", 1, 1, 1)
    head += reshdr(0, 0, 0, 0)  # offset table
    head += reshdr(len(xml), xml_flags, HEADER_SIZE, len(xml))  # XML data
    head += reshdr(0, 0, 0, 0)  # boot metadata
    head += struct.pack("<I", 0) + reshdr(0, 0, 0, 0)  # boot index, integrity
    head = head.ljust(HEADER_SIZE, b"\0")
    assert len(head) == HEADER_SIZE
    return head + xml


def build(name: str, *, xml: str, noprompt_size: int = IMAGE_SIZE, noprompt: bool = True, install: str | None = "install.wim",
          xml_flags: int = FLAG_METADATA) -> None:
    efisys, efisys_noprompt = image(1), image(2, noprompt_size)
    iso = pycdlib.PyCdlib()
    iso.new(interchange_level=3, udf="2.60")

    def add(data: bytes, iso_path: str | None, udf_path: str | None) -> None:
        iso.add_fp(io.BytesIO(data), len(data), iso_path, udf_path=udf_path)

    for udf_dir in ("/efi", "/efi/microsoft", "/efi/microsoft/boot", "/sources"):
        iso.add_directory(udf_path=udf_dir)
    # The UDF files the build reads, and the boot images the firmware loads: copies in other extents.
    add(efisys, None, "/efi/microsoft/boot/efisys.bin")
    if noprompt:
        add(efisys_noprompt, None, "/efi/microsoft/boot/efisys_noprompt.bin")
    if install:
        add(wim(xml, xml_flags, esd=install.endswith(".esd")), None, f"/sources/{install}")
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
