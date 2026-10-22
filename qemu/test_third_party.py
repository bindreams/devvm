"""Tests for the licence parsing in third_party.py, on licence fields as `pacman -Qi` prints them.

Run: uv run --with pytest pytest qemu/test_third_party.py

The strings are `%LICENSE%` entries of the MSYS2 ucrt64 package database (two constructed cases are marked). `pacman -Qi` joins the entries of one
package with two spaces; an entry is `spdx:<expression>` or `<component>:spdx:<expression>`.
"""

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent))

from third_party import Failure, classify, spdx_ids  # noqa: E402


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
