# /// script
# requires-python = ">=3.11"
# ///
"""Spike only (never merged): reads the image list out of an `install.wim` and renders the answer file for one image.

The WIM header holds the XML resource's size, flags and offset at byte 0x48 (7-byte size, 1-byte flags, 8-byte offset,
8-byte original size); the XML is UTF-16LE. The answer file takes its language from the chosen image's default language
and installs the image by NAME, so nothing in it is English-only.

Disk rule (`--disk-rule`):
  available  `winpe.cmd` (on the answer disk) loads the virtio storage driver, partitions the one fixed disk of 32 GB or
             more and Setup installs to the first available partition (`InstallToAvailablePartition`), so Windows' disk
             numbering does not matter.
  available-nopnp  `available` without the answer file's PnpCustomizationsWinPE driver paths (`winpe.cmd` already loaded
             viostor with drvload; measures whether loading it twice breaks Setup).
  available-offline  `available-nopnp` plus the driver paths in the offlineServicing pass, which Setup applies to the
             installed image before its first boot (viostor is boot-critical; drvload reaches WinPE only).
  auto       the same answer file, with no `partition.flag`: whatever Setup does with a blank disk and
             `InstallToAvailablePartition` alone.
  id:N       the answer file's own layout on disk N: only right when the numbering is what the run assumed.

Usage: render_answer.py <install.wim> <template> <answer.xml> <redacted.xml> <image.json> <wim.xml> [--name NAME]
                        [--disk-rule available|available-nopnp|available-offline|auto|id:N]
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

LAYOUT = """      <DiskConfiguration>
        <WillShowUI>OnError</WillShowUI>
        <Disk wcm:action="add">
          <DiskID>{{DISK}}</DiskID>
          <WillWipeDisk>true</WillWipeDisk>
          <CreatePartitions>
            <CreatePartition wcm:action="add"><Order>1</Order><Type>EFI</Type><Size>100</Size></CreatePartition>
            <CreatePartition wcm:action="add"><Order>2</Order><Type>MSR</Type><Size>16</Size></CreatePartition>
            <CreatePartition wcm:action="add"><Order>3</Order><Type>Primary</Type><Extend>true</Extend></CreatePartition>
          </CreatePartitions>
          <ModifyPartitions>
            <ModifyPartition wcm:action="add"><Order>1</Order><PartitionID>1</PartitionID><Label>System</Label><Format>FAT32</Format></ModifyPartition>
            <ModifyPartition wcm:action="add"><Order>2</Order><PartitionID>2</PartitionID></ModifyPartition>
            <ModifyPartition wcm:action="add"><Order>3</Order><PartitionID>3</PartitionID><Label>Windows</Label><Letter>C</Letter><Format>NTFS</Format></ModifyPartition>
          </ModifyPartitions>
        </Disk>
      </DiskConfiguration>
"""


PNP_WINPE = """    <component name="Microsoft-Windows-PnpCustomizationsWinPE" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <DriverPaths>
{{DRIVER_PATHS_PE}}
      </DriverPaths>
    </component>"""


PNP_OFFLINE = """  <settings pass="offlineServicing">
    <component name="Microsoft-Windows-PnpCustomizationsNonWinPE" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <DriverPaths>
{{DRIVER_PATHS_OFFLINE}}
      </DriverPaths>
    </component>
  </settings>"""


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


def disk_and_image(rule: str, image_name: str) -> str:
    install_from = (
        "          <InstallFrom>\n"
        '            <MetaData wcm:action="add">\n'
        "              <Key>/IMAGE/NAME</Key>\n"
        f"              <Value>{escape(image_name)}</Value>\n"
        "            </MetaData>\n"
        "          </InstallFrom>\n"
    )
    if rule in ("available", "available-nopnp", "available-offline", "auto"):
        install_to = "          <InstallToAvailablePartition>true</InstallToAvailablePartition>\n"
        layout = ""
    elif rule.startswith("id:"):
        disk = int(rule[3:])
        install_to = (
            f"          <InstallTo>\n            <DiskID>{disk}</DiskID>\n            <PartitionID>3</PartitionID>\n"
            "          </InstallTo>\n"
        )
        layout = LAYOUT.replace("{{DISK}}", str(disk))
    else:
        raise SystemExit(f"unknown disk rule {rule!r}")
    return layout + "      <ImageInstall>\n        <OSImage>\n" + install_to + install_from + "        </OSImage>\n      </ImageInstall>"


def option(args: list[str], flag: str, default: str | None) -> str | None:
    if flag not in args:
        return default
    at = args.index(flag)
    value = args[at + 1]
    del args[at : at + 2]
    return value


def main() -> None:
    args = sys.argv[1:]
    name = option(args, "--name", None)
    rule = option(args, "--disk-rule", "available")
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
        "{{DISK_AND_IMAGE}}": disk_and_image(str(rule), image["name"]),
        "{{PNP_OFFLINE}}": (
            PNP_OFFLINE.replace("{{DRIVER_PATHS_OFFLINE}}", driver_paths("off")) if rule == "available-offline" else ""
        ),
        "{{PNP_WINPE}}": "" if rule in ("available-nopnp", "available-offline") else PNP_WINPE.replace("{{DRIVER_PATHS_PE}}", driver_paths("pe")),
        "{{DRIVER_PATHS}}": driver_paths("os"),
    }
    for key, value in filled.items():
        text = text.replace(key, value)
    answer.write_text(text.replace("{{PASSWORD}}", password), encoding="utf-8")
    redacted.write_text(text.replace("{{PASSWORD}}", "REDACTED"), encoding="utf-8")
    print(f"chosen: {image}; disk rule: {rule}")


main()
