# /// script
# requires-python = ">=3.11"
# ///
"""Spike only (never merged): reads the image list out of an `install.wim` and renders the answer file for one image.

The WIM header holds the XML resource's size, flags and offset at byte 0x48 (7-byte size, 1-byte flags, 8-byte offset,
8-byte original size); the XML is UTF-16LE. The answer file takes its language from the chosen image's default language
and installs the image by NAME, so nothing in it is English-only.

Usage: render_answer.py <install.wim> <template> <answer.xml> <redacted.xml> <image.json> <wim.xml> [--name NAME]
"""

import json
import secrets
import struct
import sys
import xml.etree.ElementTree as ET
from pathlib import Path
from xml.sax.saxutils import escape

COMPRESSED = 0x04
VIRTIO_LETTERS = "DEFGH"
VIRTIO_DIRS = ("viostor", "NetKVM")


def wim_xml(path: Path) -> bytes:
    with path.open("rb") as f:
        header = f.read(0x78)
        if header[:8] != b"MSWIM\0\0\0":
            raise SystemExit(f"{path} is not a WIM")
        size_flags, offset, _original = struct.unpack_from("<QQQ", header, 0x48)
        flags = size_flags >> 56
        size = size_flags & 0x00FFFFFFFFFFFFFF
        if flags & COMPRESSED:
            raise SystemExit("the WIM's XML resource is compressed; this spike reads only an uncompressed one")
        f.seek(offset)
        return f.read(size)


def images(xml_bytes: bytes) -> list[dict]:
    root = ET.fromstring(xml_bytes.decode("utf-16-le").lstrip("﻿"))
    found = []
    for image in root.findall("IMAGE"):
        windows = image.find("WINDOWS")
        found.append(
            {
                "index": int(image.get("INDEX")),
                "name": image.findtext("NAME"),
                "description": image.findtext("DESCRIPTION"),
                "edition_id": windows.findtext("EDITIONID") if windows is not None else None,
                "arch": windows.findtext("ARCH") if windows is not None else None,
                "default_language": windows.findtext("LANGUAGES/DEFAULT") if windows is not None else None,
                "build": windows.findtext("VERSION/BUILD") if windows is not None else None,
            }
        )
    return found


def driver_paths(prefix: str) -> str:
    entries = []
    key = 1
    for letter in VIRTIO_LETTERS:
        for directory in VIRTIO_DIRS:
            entries.append(
                f'        <PathAndCredentials wcm:action="add" wcm:keyValue="{prefix}{key}">'
                f"<Path>{letter}:\\{directory}\\w11\\amd64</Path></PathAndCredentials>"
            )
            key += 1
    return "\n".join(entries)


def main() -> None:
    args = sys.argv[1:]
    name = None
    if "--name" in args:
        at = args.index("--name")
        name = args[at + 1]
        del args[at : at + 2]
    wim, template, answer, redacted, image_json, xml_out = map(Path, args)

    raw = wim_xml(wim)
    xml_out.write_bytes(raw)
    found = images(raw)
    print(json.dumps(found, indent=2))
    if name is not None:
        chosen = [i for i in found if i["name"] == name]
    elif len(found) == 1:
        chosen = found
    else:
        raise SystemExit("several images and no --name: " + ", ".join(repr(i["name"]) for i in found))
    if len(chosen) != 1:
        raise SystemExit(f"expected exactly one image, got {len(chosen)}")
    image = chosen[0]
    if image["arch"] != "9":
        raise SystemExit(f"image {image['name']!r} has ARCH {image['arch']}, not x64 (9)")
    image_json.write_text(json.dumps(image, indent=2) + "\n", encoding="utf-8")

    password = "Aa1-" + secrets.token_urlsafe(18)
    text = template.read_text(encoding="utf-8")
    filled = {
        "{{LANG}}": image["default_language"],
        "{{IMAGE_NAME}}": escape(image["name"]),
        "{{DRIVER_PATHS_PE}}": driver_paths("pe"),
        "{{DRIVER_PATHS}}": driver_paths("os"),
    }
    for key, value in filled.items():
        text = text.replace(key, value)
    answer.write_text(text.replace("{{PASSWORD}}", password), encoding="utf-8")
    redacted.write_text(text.replace("{{PASSWORD}}", "REDACTED"), encoding="utf-8")
    print(f"chosen: {image}")


main()
