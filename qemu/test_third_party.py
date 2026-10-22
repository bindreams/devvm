"""Tests for the licence parsing in third_party.py, on licence fields as `pacman -Qi` prints them.

Run: uv run --with pytest pytest qemu/test_third_party.py

The strings are `%LICENSE%` entries of the MSYS2 ucrt64 package database (two constructed cases are marked). `pacman -Qi` joins the entries of one
package with two spaces; an entry is `spdx:<expression>` or `<component>:spdx:<expression>`.
"""

import os
import subprocess
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent))

from third_party import (  # noqa: E402
    Failure,
    Package,
    FIRMWARE_LICENCES,
    classify,
    copy_firmware_licences,
    licence_files,
    msys_prefix,
    share_inventory,
    spdx_ids,
    write_source_offer,
    write_third_party,
)


# (package, licence field as pacman -Qi prints it, SPDX ids, class). The first block is what the QEMU bundle contains.
BUNDLED = [
    ("glib2", "spdx:LGPL-2.1-or-later", ["LGPL-2.1-or-later"], "copyleft"),
    (
        "libiconv",
        "spdx:LGPL-2.1-or-later  documentation:spdx:GPL-3.0-or-later",
        ["LGPL-2.1-or-later", "GPL-3.0-or-later"],
        "copyleft",
    ),
    (
        "gettext-runtime",
        "spdx:GPL-3.0-or-later AND LGPL-2.1-or-later",
        ["GPL-3.0-or-later", "LGPL-2.1-or-later"],
        "copyleft",
    ),
    ("pcre2", "spdx:BSD-3-Clause", ["BSD-3-Clause"], "permissive"),
    ("pixman", "spdx:MIT", ["MIT"], "permissive"),
    ("libslirp", "spdx:BSD-3-Clause", ["BSD-3-Clause"], "permissive"),
    ("winpthreads", "spdx:MIT AND BSD-3-Clause", ["MIT", "BSD-3-Clause"], "permissive"),
    ("zlib", "spdx:Zlib", ["Zlib"], "permissive"),
]

OTHER = [
    (
        "gcc-libs",
        "spdx:GPL-3.0-or-later WITH GCC-exception-3.1 AND GFDL-1.3-or-later",
        ["GPL-3.0-or-later", "GFDL-1.3-or-later"],
        "copyleft",
    ),
    ("xz", "spdx:0BSD AND LGPL-2.1-or-later AND GPL-2.0-or-later", ["0BSD", "LGPL-2.1-or-later", "GPL-2.0-or-later"], "copyleft"),
    ("0bsd only", "spdx:0BSD", ["0BSD"], "permissive"),
    ("clang", "spdx:Apache-2.0 WITH LLVM-exception", ["Apache-2.0"], "permissive"),
    ("b3sum", "spdx:CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception", ["CC0-1.0", "Apache-2.0", "Apache-2.0"], "permissive"),
    ("fltk", "spdx:LGPL-2.0-or-later WITH FLTK-exception", ["LGPL-2.0-or-later"], "copyleft"),
    ("dragonbox", "spdx:Apache-2.0 WITH LLVM-exception OR BSL-1.0", ["Apache-2.0", "BSL-1.0"], "permissive"),
    # Constructed, to cover parentheses and a second component.
    ("grouped", "spdx:(MIT OR Apache-2.0) AND GPL-2.0-only", ["MIT", "Apache-2.0", "GPL-2.0-only"], "copyleft"),
    ("two components", "spdx:MIT  docs:spdx:CC0-1.0", ["MIT", "CC0-1.0"], "permissive"),
]


@pytest.mark.parametrize(("package", "field", "ids", "klass"), BUNDLED + OTHER)
def test_spdx_ids_of_a_real_msys2_field(package, field, ids, klass):
    assert spdx_ids(field) == ids


@pytest.mark.parametrize(("package", "field", "ids", "klass"), BUNDLED + OTHER)
def test_class_of_a_real_msys2_field(package, field, ids, klass):
    assert classify(field, package) == klass


@pytest.mark.parametrize(
    "field",
    [
        "",
        "   ",
        "spdx:Frobnicate-1.0",
        "spdx:MIT AND Frobnicate-1.0",
        "documentation:spdx:Frobnicate-1.0",
        "custom:BSD",
        "custom:SIL OFL1.1",
        "spdx-GPL-3.0-or-later",
        "unknown",
    ],
)
def test_an_unknown_or_missing_licence_fails_the_build(field):
    with pytest.raises(Failure):
        classify(field, "pkg")


# The licence files each bundled package installs, as the MSYS2 ucrt64 database lists them (relative to the root).
LICENCE_FILES = {
    "dtc": ["dtc/GPL"],
    "gettext-runtime": [
        "gettext-runtime/COPYING",
        "gettext-runtime/intl/COPYING.LIB",
        "gettext-runtime/libasprintf/COPYING",
        "gettext-runtime/libasprintf/COPYING.LIB",
    ],
    "glib2": ["glib2/COPYING"],
    "libiconv": [
        "libiconv/COPYING",
        "libiconv/COPYING.LIB",
        "libiconv/README",
        "libiconv/libcharset/COPYING.LIB",
    ],
    "libslirp": ["libslirp/COPYRIGHT"],
    "pcre2": ["pcre2/COPYING", "pcre2/LICENCE.md"],
    "pixman": ["pixman/COPYING"],
    "winpthreads": ["winpthreads/COPYING"],
    "zlib": ["zlib/LICENSE"],
}


def pacman_ql(package: str, files: list[str]) -> str:
    """What `pacman -Ql` prints: every directory above each file too, one `<package> <absolute path>` per line."""
    paths = ["/ucrt64/", "/ucrt64/bin/", "/ucrt64/bin/tool.exe", "/ucrt64/share/", "/ucrt64/share/licenses/"]
    for file in files:
        parts = file.split("/")
        paths += [f"/ucrt64/share/licenses/{'/'.join(parts[:i])}/" for i in range(1, len(parts))]
        paths.append(f"/ucrt64/share/licenses/{file}")
    return "\n".join(f"mingw-w64-ucrt-x86_64-{package} {path}" for path in dict.fromkeys(paths)) + "\n"


@pytest.mark.parametrize("package", LICENCE_FILES)
def test_licence_files_of_a_bundled_package(package):
    files = LICENCE_FILES[package]
    found = licence_files(pacman_ql(package, files), "/ucrt64")
    # (path under the prefix, path under licenses/<package>/): the package's own directory level is dropped.
    assert found == [(f"share/licenses/{f}", Path(f).relative_to(package)) for f in files]


def test_a_package_without_licence_files_yields_none():
    assert licence_files("pkg /ucrt64/bin/\npkg /ucrt64/bin/x.dll\n", "/ucrt64") == []


def test_the_prefix_is_posix_even_when_the_environment_holds_a_windows_path(monkeypatch):
    # An MSYS2 shell hands a native Windows program (the UCRT64 python) MSYSTEM_PREFIX as `D:/a/_temp/msys64/ucrt64`,
    # while `pacman -Ql` prints `/ucrt64/...`. /usr exists in every MSYS2 and Git for Windows tree.
    windows = subprocess.run(["cygpath", "-m", "/usr"], capture_output=True, text=True, check=True).stdout.strip()
    assert windows != "/usr"
    monkeypatch.setenv("MSYSTEM_PREFIX", windows)
    posix, native = msys_prefix()
    assert posix == "/usr"
    assert os.path.samefile(native, windows)


# The release text and the mirrored file names -------------------------------------------------------------------------


def package(version: str, klass: str = "copyleft") -> Package:
    return Package("mingw-w64-ucrt-x86_64-foo", version, "spdx:LGPL-2.1-or-later", klass, "mingw-w64-foo", ["foo.dll"])


def test_the_mirrored_name_replaces_characters_github_may_rewrite():
    # MSYS2 writes the epoch of "1:2.0-1" as "1~2.0-1"; GitHub rewrites "~" in asset names. The mirror name is
    # deterministic and uses only [A-Za-z0-9._-].
    assert package("1:2.0-1").source_asset == "mingw-w64-foo-1~2.0-1.src.tar.zst"
    assert package("1:2.0-1").mirror_asset == "mingw-w64-foo-1_2.0-1.src.tar.zst"
    assert package("2.0+r3-1").mirror_asset == "mingw-w64-foo-2.0_r3-1.src.tar.zst"
    assert package("2.0-1").mirror_asset == package("2.0-1").source_asset


def test_the_third_party_text_names_the_mirror_and_the_msys2_file_name_when_they_differ(tmp_path):
    write_third_party(tmp_path, [package("1:2.0-1")], "qemu-1.2.3-1", "o/r", ["  share/x: y"])
    text = (tmp_path / "THIRD-PARTY.txt").read_text(encoding="utf-8")
    assert "/releases/download/qemu-1.2.3-1/mingw-w64-foo-1_2.0-1.src.tar.zst" in text
    assert "MSYS2 file name: mingw-w64-foo-1~2.0-1.src.tar.zst" in text

    write_third_party(tmp_path, [package("2.0-1")], "qemu-1.2.3-1", "o/r", [])
    assert "MSYS2 file name" not in (tmp_path / "THIRD-PARTY.txt").read_text(encoding="utf-8")


def test_the_source_offer_lists_the_mirror_names(tmp_path):
    write_source_offer(tmp_path, [package("1:2.0-1")], "qemu-1.2.3-1", "o/r")
    assert "mingw-w64-foo-1_2.0-1.src.tar.zst" in (tmp_path / "SOURCE.txt").read_text(encoding="utf-8")


def test_qemu_is_gpl_2_as_a_whole_not_or_later(tmp_path):
    write_third_party(tmp_path, [], "qemu-1.2.3-1", "o/r", [])
    write_source_offer(tmp_path, [], "qemu-1.2.3-1", "o/r")
    for name in ("THIRD-PARTY.txt", "SOURCE.txt"):
        text = (tmp_path / name).read_text(encoding="utf-8")
        assert "GPL-2.0 as a whole" in text
        assert "GPL-2.0-or-later" not in text


# The firmware and ROM inventory ---------------------------------------------------------------------------------------


def tree(root: Path, *files: str) -> Path:
    for name in files:
        (root / name).parent.mkdir(parents=True, exist_ok=True)
        (root / name).write_text("x")
    return root


def source_tree(root: Path, *prebuilt: str) -> Path:
    """A stand-in for the extracted QEMU tarball with the recipe files and source directories the inventory names."""
    tree(root, "roms/Makefile", "roms/edk2-build.config")
    for directory in ("seabios", "ipxe", "qboot", "vbootrom", "edk2"):
        tree(root, f"roms/{directory}/README")
    tree(root, "pc-bios/optionrom/kvmvapic.S", "pc-bios/optionrom/linuxboot_dma.c", "pc-bios/optionrom/multiboot_dma.S")
    tree(root, "pc-bios/optionrom/pvh.S")
    return tree(root, *prebuilt)


def test_every_blob_is_listed_with_its_prebuilt_file_and_source(tmp_path):
    share = tree(tmp_path / "share", "bios-256k.bin", "edk2-x86_64-code.fd", "kvmvapic.bin", "keymaps/en-us")
    src = source_tree(
        tmp_path / "src",
        "pc-bios/bios-256k.bin",
        "pc-bios/edk2-x86_64-code.fd.bz2",
        "pc-bios/kvmvapic.bin",
        "pc-bios/keymaps/en-us",
    )
    text = "\n".join(share_inventory(share, src))
    assert "share/bios-256k.bin: pc-bios/bios-256k.bin, source roms/seabios" in text
    assert "share/edk2-x86_64-code.fd: pc-bios/edk2-x86_64-code.fd.bz2, source roms/edk2" in text
    assert "share/kvmvapic.bin: pc-bios/kvmvapic.bin, source pc-bios/optionrom" in text
    assert "share/keymaps/: pc-bios/keymaps/, source pc-bios/keymaps" in text


def test_a_file_in_share_with_no_inventory_entry_fails(tmp_path):
    share = tree(tmp_path / "share", "mystery.bin")
    with pytest.raises(Failure, match=r"share/mystery\.bin"):
        share_inventory(share, source_tree(tmp_path / "src", "pc-bios/mystery.bin"))


def test_an_empty_directory_in_share_fails(tmp_path):
    share = tmp_path / "share"
    (share / "keymaps").mkdir(parents=True)
    with pytest.raises(Failure, match="holds no file"):
        share_inventory(share, source_tree(tmp_path / "src"))


def test_a_blob_missing_from_the_tarball_fails(tmp_path):
    share = tree(tmp_path / "share", "bios-256k.bin")
    with pytest.raises(Failure, match=r"pc-bios/bios-256k\.bin"):
        share_inventory(share, source_tree(tmp_path / "src"))


def test_a_source_directory_missing_from_the_tarball_fails(tmp_path):
    share = tree(tmp_path / "share", "bios-256k.bin")
    src = source_tree(tmp_path / "src", "pc-bios/bios-256k.bin")
    (src / "roms/seabios/README").unlink()
    (src / "roms/seabios").rmdir()
    with pytest.raises(Failure, match="roms/seabios"):
        share_inventory(share, src)


def test_a_source_directory_with_no_file_in_it_fails(tmp_path):
    # An unpopulated submodule directory exists but holds nothing to build from.
    share = tree(tmp_path / "share", "bios-256k.bin")
    src = source_tree(tmp_path / "src", "pc-bios/bios-256k.bin")
    (src / "roms/seabios/README").unlink()
    with pytest.raises(Failure, match="roms/seabios"):
        share_inventory(share, src)


def test_the_third_party_text_says_each_source_directory_holds_its_own_recipe(tmp_path):
    write_third_party(tmp_path, [], "qemu-1.2.3-1", "o/r", [])
    text = (tmp_path / "THIRD-PARTY.txt").read_text(encoding="utf-8")
    assert "roms/Makefile" in text and "roms/edk2-build.config" in text
    assert "pc-bios/optionrom/Makefile" in text
    assert "its own build recipe" in text


# The licence texts of the firmware projects ---------------------------------------------------------------------------


def firmware_source(root: Path) -> Path:
    for project, (directory, names) in FIRMWARE_LICENCES.items():
        for name in names:
            tree(root, f"{directory}/{name}")
            (root / directory / name).write_text(f"{project}/{name}")
    return root


def test_the_firmware_licences_are_the_ones_the_review_named():
    listed = {project: set(names) for project, (_, names) in FIRMWARE_LICENCES.items()}
    assert listed == {
        "seabios": {"COPYING", "COPYING.LESSER"},
        "ipxe": {"COPYING", "COPYING.GPLv2", "COPYING.UBDL"},
        "vbootrom": {"LICENSE"},
        "qboot": {"LICENSE"},
    }
    assert FIRMWARE_LICENCES["seabios"][0] == "roms/seabios"


def test_the_firmware_licence_texts_are_copied_under_licenses_qemu_firmware(tmp_path):
    src = firmware_source(tmp_path / "src")
    stage = tmp_path / "stage"
    copy_firmware_licences(stage, src)
    assert (stage / "licenses/qemu-firmware/seabios/COPYING.LESSER").read_text() == "seabios/COPYING.LESSER"
    assert (stage / "licenses/qemu-firmware/ipxe/COPYING.UBDL").read_text() == "ipxe/COPYING.UBDL"
    assert (stage / "licenses/qemu-firmware/vbootrom/LICENSE").read_text() == "vbootrom/LICENSE"
    assert (stage / "licenses/qemu-firmware/qboot/LICENSE").read_text() == "qboot/LICENSE"


def test_a_missing_firmware_licence_file_fails_and_names_it(tmp_path):
    src = firmware_source(tmp_path / "src")
    (src / "roms/ipxe/COPYING.UBDL").unlink()
    with pytest.raises(Failure, match=r"roms/ipxe/COPYING\.UBDL"):
        copy_firmware_licences(tmp_path / "stage", src)


def test_the_third_party_text_names_the_firmware_licence_directory(tmp_path):
    write_third_party(tmp_path, [], "qemu-1.2.3-1", "o/r", [])
    text = (tmp_path / "THIRD-PARTY.txt").read_text(encoding="utf-8")
    assert "licenses/qemu-firmware/<project>/" in text
    for project in FIRMWARE_LICENCES:
        assert project in text
