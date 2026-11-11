# /// script
# requires-python = ">=3.11"
# dependencies = ["pycdlib"]
# ///
"""Spike only (never merged): makes a Windows ISO's UEFI El Torito boot image the no-prompt one, in a copy or in place.

A Windows ISO boots UEFI through efi/microsoft/boot/efisys.bin, which asks "Press any key to boot from CD or DVD".
The same ISO carries efisys_noprompt.bin, Microsoft's prompt-free variant of that image. The boot catalog points at the
extent of efisys.bin, so this overwrites that extent with the no-prompt image. Nothing depends on timing.

The extent is not computed from the file systems: it is the boot catalog's load address, accepted only if the bytes
there equal efisys.bin as the file system hands it out.

Usage: patch_iso.py <in.iso> <out.iso>   (the same path twice patches in place)
       patch_iso.py --extent <in.iso> <noprompt.bin>   (changes nothing: writes the no-prompt image, prints its extent)
"""

import io
import shutil
import sys
from pathlib import Path

import pycdlib

SECTOR = 2048
DIRECTORY = "efi/microsoft/boot"


def fetch(iso: pycdlib.PyCdlib, name: str) -> bytes:
    """The bytes of `efi/microsoft/boot/<name>`, from whichever of the ISO's file systems has the file."""
    tried = []
    for kind in ("udf_path", "joliet_path", "iso_path"):
        for text in (f"/{DIRECTORY}/{name}", f"/{DIRECTORY}/{name}".upper()):
            path = f"{text};1" if kind == "iso_path" else text
            out = io.BytesIO()
            try:
                iso.get_file_from_iso_fp(out, **{kind: path})
            except Exception as error:  # pycdlib has no common base for "not found" across file systems
                tried.append(f"{kind}={path}: {error}")
                continue
            print(f"{name}: found by {kind}={path}, {len(out.getvalue())} bytes")
            return out.getvalue()
    raise SystemExit(f"{name} was not found:\n" + "\n".join(tried))


def main() -> None:
    extent_only = sys.argv[1] == "--extent"
    if extent_only:
        src, bin_out = sys.argv[2:4]
        dst = src
    else:
        src, dst = sys.argv[1:3]
        if Path(src).resolve() != Path(dst).resolve():
            shutil.copyfile(src, dst)
    iso = pycdlib.PyCdlib()
    iso.open(dst)
    prompt = fetch(iso, "efisys.bin")
    quiet = fetch(iso, "efisys_noprompt.bin")
    if len(prompt) != len(quiet):
        raise SystemExit(f"the boot images differ in size ({len(prompt)} and {len(quiet)}); refusing to guess")

    catalog = iso.eltorito_boot_catalog
    entries = [catalog.initial_entry] + [e for section in catalog.sections for e in section.section_entries]
    loads = [entry.load_rba for entry in entries]
    print("boot catalog load_rba values:", loads)
    iso.close()

    with open(dst, "r+b" if not extent_only else "rb") as f:
        for load in loads:
            f.seek(load * SECTOR)
            if f.read(len(prompt)) == prompt:
                if extent_only:
                    Path(bin_out).write_bytes(quiet)
                    print(f"EXTENT sector={load} offset={load * SECTOR} length={len(prompt)}")
                    return
                f.seek(load * SECTOR)
                f.write(quiet)
                print(f"patched: the boot image at sector {load} now holds efisys_noprompt.bin")
                return
    raise SystemExit("no boot catalog entry points at the bytes of efisys.bin; refusing to guess")


main()
