#!/usr/bin/env python3
"""Document and mirror the MSYS2 libraries bundled next to QEMU. Runs in the MSYS2 UCRT64 shell, after `pe_deps.py
bundle` has copied the DLLs into `<stage>/bin`.

    third_party.py --stage <dir> --sources <dir> --tag <release tag> --repo <owner/repo>

For every bundled DLL: find the owning package, classify its licence (an unknown or missing licence fails), copy its
licence files into `<stage>/licenses/<package>/`, and for copyleft packages download the exact source archive of the
installed version into `<sources>` (it becomes a release asset). Writes `THIRD-PARTY.txt` and `SOURCE.txt` into the
stage. Standard library only.
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

SOURCES_URL = "https://repo.msys2.org/mingw/sources"
RECIPES_URL = "https://github.com/msys2/MINGW-packages/tree/master"
PREFIX = "mingw-w64-ucrt-x86_64-"

# Licence families whose terms require offering the source: mirrored.
COPYLEFT = ("GPL", "LGPL", "AGPL", "MPL", "EPL", "CDDL", "EUPL", "OSL")
# Licences that ask only for attribution. Anything else is unknown and fails the build: add it here after reading it.
PERMISSIVE = (
    "MIT", "BSD", "ISC", "ZLIB", "APACHE", "BSL", "CC0", "UNLICENSE", "X11", "LIBPNG", "CURL", "HPND", "UNICODE",
    "OPENSSL", "BZIP2", "FTL", "IJG",
)  # fmt: skip


HERE = Path(__file__).resolve().parent
QEMU_VERSION = dict(line.split("=", 1) for line in (HERE / "pin.env").read_text().splitlines() if "=" in line)[
    "QEMU_VERSION"
]


class Failure(Exception):
    """A condition that must stop the build, with the message to show."""


@dataclass
class Package:
    name: str
    version: str
    licenses: str
    klass: str
    base: str
    dlls: list[str] = field(default_factory=list)

    @property
    def short(self) -> str:
        return self.name.removeprefix(PREFIX)

    @property
    def source_asset(self) -> str:
        # MSYS2 writes an epoch "1:2.0-1" as "1~2.0-1" in file names.
        return f"{self.base}-{self.version.replace(':', '~')}.src.tar.zst"


# Everything that touches the system is below, so a check can replace it ---------------------------------------------


def pacman(*args: str) -> str:
    done = subprocess.run(["pacman", *args], capture_output=True, text=True)
    if done.returncode != 0:
        raise Failure(f"pacman {' '.join(args)} failed: {done.stderr.strip()}")
    return done.stdout


def msys_prefix() -> tuple[str, Path]:
    """The MSYSTEM prefix as the MSYS2 shell writes it ("/ucrt64") and as Windows writes it."""
    prefix = os.environ["MSYSTEM_PREFIX"]
    native = subprocess.run(["cygpath", "-w", prefix], capture_output=True, text=True, check=True).stdout.strip()
    return prefix, Path(native)


def package_base(name: str, version: str) -> str:
    """pkgbase of an installed package: `%BASE%` in its pacman database entry (`pacman -Qi` does not show it)."""
    local = subprocess.run(["cygpath", "-w", "/var/lib/pacman/local"], capture_output=True, text=True, check=True)
    for desc in Path(local.stdout.strip()).glob(f"{name}-*/desc"):
        fields = parse_desc(desc.read_text(encoding="utf-8"))
        if fields.get("NAME") == name and fields.get("BASE"):
            return fields["BASE"]
    raise Failure(f"no pkgbase in the pacman database for {name} {version}")


def fetch(url: str, dest: Path) -> None:
    done = subprocess.run(["curl", "-fsSL", "-o", str(dest), url], capture_output=True, text=True)
    if done.returncode != 0:
        raise Failure(f"cannot download {url}: {done.stderr.strip() or done.returncode}")


# Parsing and classification ----------------------------------------------------------------------------------------


def parse_desc(text: str) -> dict[str, str]:
    fields: dict[str, str] = {}
    for block in text.split("\n\n"):
        lines = block.strip().splitlines()
        if len(lines) >= 2 and lines[0].startswith("%") and lines[0].endswith("%"):
            fields[lines[0].strip("%")] = lines[1].strip()
    return fields


def parse_info(text: str) -> dict[str, str]:
    fields = {}
    for line in text.splitlines():
        match = re.match(r"^(\S[^:]*?)\s*:\s*(.*)$", line)
        if match:
            fields[match.group(1)] = match.group(2).strip()
    return fields


def classify(licenses: str, package: str) -> str:
    """"copyleft" or "permissive"; raises Failure for an empty or unrecognised licence."""
    tokens = []
    for token in re.split(r"[\s(),]+", licenses):
        token = re.sub(r"^spdx:", "", token, flags=re.IGNORECASE)
        # Exception identifiers (GCC-exception-3.1, ...) qualify a licence listed beside them.
        if token and token.upper() not in ("AND", "OR", "WITH", "NONE", "UNKNOWN") and "exception" not in token.lower():
            tokens.append(token)
    if not tokens:
        raise Failure(f"{package}: pacman -Qi shows no licence ({licenses!r})")

    unknown = [t for t in tokens if not t.upper().startswith(COPYLEFT + PERMISSIVE)]
    if unknown:
        raise Failure(f"{package}: unrecognised licence {', '.join(unknown)} in {licenses!r}; classify it in third_party.py")
    return "copyleft" if any(t.upper().startswith(COPYLEFT) for t in tokens) else "permissive"


# The work ----------------------------------------------------------------------------------------------------------


def collect(stage: Path) -> list[Package]:
    prefix, _ = msys_prefix()
    packages: dict[str, Package] = {}
    for dll in sorted((stage / "bin").glob("*.dll")):
        owner = pacman("-Qoq", f"{prefix}/bin/{dll.name}").strip()
        if not owner:
            raise Failure(f"{dll.name} is owned by no package")
        if owner not in packages:
            info = parse_info(pacman("-Qi", owner))
            version = info.get("Version")
            if not version:
                raise Failure(f"{owner}: pacman -Qi shows no version")
            licenses = info.get("Licenses", "")
            packages[owner] = Package(
                owner, version, licenses, classify(licenses, owner), package_base(owner, version)
            )
        packages[owner].dlls.append(dll.name)
    return list(packages.values())


def copy_licenses(stage: Path, packages: list[Package]) -> None:
    prefix, native = msys_prefix()
    root = f"{prefix}/share/licenses/"
    for package in packages:
        count = 0
        for line in pacman("-Ql", package.name).splitlines():
            path = line.split(" ", 1)[1] if " " in line else ""
            if not path.startswith(root) or path.endswith("/"):
                continue
            relative = path[len(prefix) + 1 :]
            inner = Path(path[len(root) :])
            target = stage / "licenses" / package.short / Path(*inner.parts[1:] or inner.parts[-1:])
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(native / relative, target)
            count += 1
        if count == 0:
            raise Failure(f"{package.name}: the package installs no licence file under share/licenses")


def mirror_sources(sources: Path, packages: list[Package]) -> None:
    sources.mkdir(parents=True, exist_ok=True)
    for package in packages:
        target = sources / package.source_asset
        if package.klass == "copyleft" and not target.exists():
            fetch(f"{SOURCES_URL}/{package.source_asset}", target)


def write_third_party(stage: Path, packages: list[Package], tag: str, repo: str) -> None:
    release = f"https://github.com/{repo}/releases/download/{tag}"
    lines = [
        "Third-party components in this QEMU build",
        "=========================================",
        "",
        "QEMU (GPL-2.0-or-later; see licenses/qemu/)",
        f"  Source: {release}/qemu-{QEMU_VERSION}.tar.xz, the exact tarball this build was made from.",
        "  Firmware: share/qemu/edk2-*.fd are the prebuilt edk2 binaries shipped inside the QEMU tarball, not rebuilt",
        "  here. Their source is under roms/edk2 in that tarball; licences: licenses/qemu/edk2-licenses.txt.",
        "",
        "Libraries from MSYS2 (bin/*.dll), each built from the MSYS2 recipe and source archive named below.",
        "",
    ]
    for package in sorted(packages, key=lambda p: p.name):
        lines += [
            f"{package.short} {package.version}",
            f"  Licence: {package.licenses} ({package.klass})",
            f"  Files: {', '.join(sorted(package.dlls))}",
            f"  Licence texts: licenses/{package.short}/",
            f"  MSYS2 recipe: {RECIPES_URL}/{package.base}",
        ]
        if package.klass == "copyleft":
            lines.append(f"  Source archive: {release}/{package.source_asset}")
        else:
            lines.append("  Source archive: not mirrored (permissive licence)")
        lines.append("")
    (stage / "THIRD-PARTY.txt").write_text("\n".join(lines), encoding="utf-8", newline="\n")


def write_source_offer(stage: Path, packages: list[Package], tag: str, repo: str) -> None:
    release = f"https://github.com/{repo}/releases/tag/{tag}"
    mirrored = [p.source_asset for p in sorted(packages, key=lambda p: p.name) if p.klass == "copyleft"]
    text = f"""Source code for this build
==========================

The complete corresponding source of everything in this archive is attached to the release it came from:

  {release}

  - qemu-{QEMU_VERSION}.tar.xz and .sig: the upstream QEMU source (GPL-2.0-or-later), signature included. The build
    patches this tarball in one place only: qemu/build.sh adds devvm.manifest to the source root and one line to
    version.rc (it embeds the UTF-8 application manifest).
  - {tag}-build-scripts.tar.gz: qemu/ and .github/workflows/qemu.yaml at the commit that built it (BUILD-INFO.txt
    names the commit): the pin, the build script, the configure options and the workflow.
  - The source archives of the bundled LGPL/GPL libraries, exactly the versions in bin/:
{chr(10).join(f"      {name}" for name in mirrored) or "      (none)"}
  - {tag}-qapi.zip: QEMU's QAPI schema (qapi/*.json).

The libraries are separate DLLs beside the executables. A modified build of the same source can replace a DLL in bin/
(LGPL section 6). If a release asset is ever unavailable, ask for the source at
https://github.com/{repo}/issues and it will be provided for three years from the release date.
"""
    (stage / "SOURCE.txt").write_text(text, encoding="utf-8", newline="\n")


def generate(stage: Path, sources: Path, tag: str, repo: str) -> None:
    packages = collect(stage)
    copy_licenses(stage, packages)
    mirror_sources(sources, packages)
    write_third_party(stage, packages, tag, repo)
    write_source_offer(stage, packages, tag, repo)
    for package in packages:
        print(f"{package.short} {package.version}: {package.licenses} ({package.klass}), {len(package.dlls)} DLLs")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stage", type=Path, required=True)
    parser.add_argument("--sources", type=Path, required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--repo", required=True)
    args = parser.parse_args()
    try:
        generate(args.stage, args.sources, args.tag, args.repo)
    except Failure as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
