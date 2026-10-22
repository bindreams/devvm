#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pefile"]
# ///
"""Walk the DLL imports of QEMU's Windows executables.

`bundle` copies every DLL the executables in `<stage>/bin` need (transitively, normal and delay-load imports) from a
search directory. `check` verifies an extracted release zip the same way and also requires the UTF-8 active-code-page
manifest in every executable. Both use one rule for what counts as a system DLL: the explicit allowlist below, never
"a file that happens to exist in the runner's System32".
"""

import argparse
import re
import shutil
import sys
from pathlib import Path

import pefile

# DLLs that ship with every supported Windows (10 1903+ / 11), by name. WinHvPlatform and WinHvEmulation are loaded at
# run time by the WHPX accelerator and exist only when the Hypervisor Platform feature is on; they are never bundled.
SYSTEM_DLLS = frozenset(
    {
        "advapi32.dll",
        "bcrypt.dll",
        "bcryptprimitives.dll",
        "cfgmgr32.dll",
        "combase.dll",
        "comdlg32.dll",
        "crypt32.dll",
        "dbghelp.dll",
        "dnsapi.dll",
        "dwmapi.dll",
        "gdi32.dll",
        "gdi32full.dll",
        "imm32.dll",
        "iphlpapi.dll",
        "kernel32.dll",
        "kernelbase.dll",
        "mpr.dll",
        "msvcrt.dll",
        "mswsock.dll",
        "netapi32.dll",
        "normaliz.dll",
        "ntdll.dll",
        "ole32.dll",
        "oleaut32.dll",
        "opengl32.dll",
        "psapi.dll",
        "rpcrt4.dll",
        "sechost.dll",
        "secur32.dll",
        "setupapi.dll",
        "shcore.dll",
        "shell32.dll",
        "shlwapi.dll",
        "ucrtbase.dll",
        "user32.dll",
        "userenv.dll",
        "usp10.dll",
        "uxtheme.dll",
        "version.dll",
        "win32u.dll",
        "winhvemulation.dll",
        "winhvplatform.dll",
        "winmm.dll",
        "winspool.drv",
        "wintrust.dll",
        "ws2_32.dll",
        "wsock32.dll",
        "wtsapi32.dll",
    }
)

# API sets are defined by name by the OS loader and have no file of their own.
API_SET = re.compile(r"^(api-ms-win|ext-ms-win)-[a-z0-9-]+\.dll$")

# Redistributable runtimes. Present on a developer or CI machine, absent on a bare one: never "system".
REJECTED = re.compile(r"^(vcruntime|msvcp|msvcr(?!t\.dll$)|concrt|vcomp|mfc)")

MANIFEST_OK = re.compile(r"<activeCodePage\b[^>]*>\s*UTF-8\s*</activeCodePage>", re.IGNORECASE)


def is_system(name: str) -> bool:
    return name in SYSTEM_DLLS or API_SET.match(name) is not None


def imports(path: Path) -> list[str]:
    """Lowercase DLL names imported by `path`, normal and delay-load."""
    pe = pefile.PE(str(path), fast_load=True)
    try:
        pe.parse_data_directories(
            directories=[
                pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_IMPORT"],
                pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT"],
            ]
        )
        entries = list(getattr(pe, "DIRECTORY_ENTRY_IMPORT", [])) + list(getattr(pe, "DIRECTORY_ENTRY_DELAY_IMPORT", []))
        return [e.dll.decode("ascii").lower() for e in entries]
    finally:
        pe.close()


def manifest_text(path: Path) -> str | None:
    """The embedded RT_MANIFEST resource of `path`, or None."""
    pe = pefile.PE(str(path), fast_load=True)
    try:
        pe.parse_data_directories(directories=[pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_RESOURCE"]])
        resources = getattr(pe, "DIRECTORY_ENTRY_RESOURCE", None)
        if resources is None:
            return None
        for kind in resources.entries:
            if kind.id != pefile.RESOURCE_TYPE["RT_MANIFEST"]:
                continue
            for name in kind.directory.entries:
                for language in name.directory.entries:
                    data = language.data.struct
                    return pe.get_data(data.OffsetToData, data.Size).decode("utf-8", errors="replace")
        return None
    finally:
        pe.close()


def closure(bin_dir: Path, search: Path | None) -> tuple[list[str], list[str]]:
    """Walk the imports of every executable in `bin_dir`.

    Returns (copied, errors). With `search`, a DLL found there is copied into `bin_dir` and walked in turn.
    """
    local = {p.name.lower(): p for p in bin_dir.iterdir() if p.suffix.lower() in (".exe", ".dll")}
    available = {p.name.lower(): p for p in search.iterdir()} if search is not None else {}
    queue = sorted(p for p in local.values() if p.suffix.lower() == ".exe")
    seen = {p.name.lower() for p in queue}
    copied: list[str] = []
    errors: list[str] = []

    while queue:
        current = queue.pop()
        for name in imports(current):
            if REJECTED.match(name):
                errors.append(f"{current.name}: imports {name}, a Visual C++ runtime that is not shipped with Windows")
            elif name in local:
                if name not in seen:
                    seen.add(name)
                    queue.append(local[name])
            elif is_system(name):
                continue
            elif name in available:
                target = bin_dir / available[name].name
                shutil.copy2(available[name], target)
                local[name] = target
                seen.add(name)
                copied.append(target.name)
                queue.append(target)
            else:
                where = "the zip" if search is None else f"the search directory {search}"
                errors.append(f"{current.name}: imports {name}, which is not in {where} and not an allowlisted system DLL")
    return sorted(copied), sorted(set(errors))


def cmd_bundle(args: argparse.Namespace) -> int:
    bin_dir = args.stage / "bin"
    copied, errors = closure(bin_dir, args.search)
    for name in copied:
        print(f"bundled: {name}")
    for error in errors:
        print(f"FAIL: {error}")
    return 1 if errors else 0


def cmd_check(args: argparse.Namespace) -> int:
    bin_dir = args.root / "bin"
    executables = sorted(bin_dir.glob("*.exe"))
    if not executables:
        print(f"FAIL: no executables in {bin_dir}")
        return 1

    _, errors = closure(bin_dir, None)
    for exe in executables:
        text = manifest_text(exe)
        if text is None:
            errors.append(f"{exe.name}: no embedded application manifest")
        elif MANIFEST_OK.search(text) is None:
            errors.append(f"{exe.name}: manifest lacks <activeCodePage>UTF-8</activeCodePage>")
        else:
            print(f"ok: {exe.name} activeCodePage UTF-8")
    for error in errors:
        print(f"FAIL: {error}")
    if not errors:
        print(f"ok: {len(executables)} executables, every import in the zip or on the system allowlist")
    return 1 if errors else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    bundle = sub.add_parser("bundle", help="copy the DLL closure of <stage>/bin/*.exe from --search")
    bundle.add_argument("--stage", type=Path, required=True)
    bundle.add_argument("--search", type=Path, required=True)
    bundle.set_defaults(func=cmd_bundle)

    check = sub.add_parser("check", help="verify an extracted zip: closure and UTF-8 manifest")
    check.add_argument("root", type=Path)
    check.set_defaults(func=cmd_check)

    args = parser.parse_args()
    sys.stdout.reconfigure(encoding="utf-8", errors="backslashreplace")
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
