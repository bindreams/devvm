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

from third_party import Failure, classify, licence_files, msys_prefix, spdx_ids  # noqa: E402


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
