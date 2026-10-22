#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Release assets for the QEMU build: the zip, its checksum, and the files that go with it.

    assets.py zip    --stage <dir> --out <zip>
    assets.py sha256 <file>
"""

import argparse
import hashlib
import shutil
import sys
import zipfile
from pathlib import Path

# What devvm reads out of the zip (the Interfaces of the unit plan), plus the files that make it redistributable.
REQUIRED = (
    "bin/qemu-system-x86_64.exe",
    "bin/qemu-system-aarch64.exe",
    "bin/qemu-img.exe",
    "share/qemu/edk2-x86_64-code.fd",
    "share/qemu/edk2-i386-vars.fd",
    "share/qemu/edk2-aarch64-code.fd",
    "share/qemu/edk2-arm-vars.fd",
    "licenses/qemu/COPYING",
    "BUILD-INFO.txt",
)


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def cmd_zip(args: argparse.Namespace) -> int:
    missing = [name for name in REQUIRED if not (args.stage / name).is_file()]
    if missing:
        print(f"FAIL: the stage lacks {', '.join(missing)}")
        return 1

    files = sorted(p for p in args.stage.rglob("*") if p.is_file())
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(args.out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for path in files:
            # A fixed timestamp makes the same stage zip to the same bytes.
            info = zipfile.ZipInfo(path.relative_to(args.stage).as_posix(), date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            with path.open("rb") as src, archive.open(info, "w") as dst:
                shutil.copyfileobj(src, dst, 1 << 20)
    print(f"{args.out} {args.out.stat().st_size} bytes, {len(files)} files, sha256 {sha256_of(args.out)}")
    return 0


def cmd_sha256(args: argparse.Namespace) -> int:
    digest = sha256_of(args.file)
    target = args.file.with_name(args.file.name + ".sha256")
    target.write_text(f"{digest} *{args.file.name}\n", newline="\n")
    print(digest)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    zip_ = sub.add_parser("zip", help="zip a stage directory")
    zip_.add_argument("--stage", type=Path, required=True)
    zip_.add_argument("--out", type=Path, required=True)
    zip_.set_defaults(func=cmd_zip)

    digest = sub.add_parser("sha256", help="write <file>.sha256 in sha256sum format and print the hash")
    digest.add_argument("file", type=Path)
    digest.set_defaults(func=cmd_sha256)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
