#!/usr/bin/env bash
# Spike only (never merged). The runner side of E1 of the wedge root-cause run: the call stacks of the hang dumps the guest
# uploaded (hangdump.ps1, wedge_driver.py), read with the release's symbols.
#   - cdb comes with the Windows SDK's debugging tools (the Server 2025 image installs them; if the directory is absent, the
#     image's own step is repeated from the SDK bundle's cache);
#   - OpenSSH-Win64_Symbols.zip is the 10.4.0.0p1-Preview release's, pinned by sha256; Microsoft's public symbol server
#     supplies ntdll, kernelbase and the others.
# Output per dump, in <out dir>: <stage>_<dump>.stack.txt (cdb-stacks.txt: every thread's stack) and
# <stage>_<dump>.fds.txt (cdb-fds.txt, best effort). Nothing here fails the run: the dumps are uploaded either way.
# Usage: stacks.sh <dump root> <out dir> <scratch dir for the symbols and their cache>
set -Eeuo pipefail
exec 2>&1
trap 'echo "stacks.sh: failed at line $LINENO, exit $?"' ERR

readonly SYMBOLS_URL='https://github.com/PowerShell/Win32-OpenSSH/releases/download/10.4.0.0p1-Preview/OpenSSH-Win64_Symbols.zip'
readonly SYMBOLS_SHA256='8e7cd6b7f6a003d186fe3c11993219a53d987b7aa009833dc043fa8f457685a5'

dump_root=$1
out_dir=$2
scratch=$3
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
mkdir -p "$out_dir" "$scratch"

if [[ -z "$(find "$dump_root" -name '*.dmp' -print -quit 2>/dev/null)" ]]; then
    echo "no dumps under $dump_root"
    exit 0
fi

find_cdb() {
    local candidate
    for candidate in \
        '/c/Program Files (x86)/Windows Kits/10/Debuggers/x64/cdb.exe' \
        '/c/Program Files/Windows Kits/10/Debuggers/x64/cdb.exe'; do
        if [[ -x "$candidate" ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

# The image's own step (runner-images Install-VisualStudio.ps1): the SDK bundle's cached installer adds the debuggers.
install_debuggers() {
    pwsh -NoProfile -Command '
        $entry = Get-ItemProperty HKLM:\Software\Wow6432Node\Microsoft\Windows\CurrentVersion\Uninstall\* |
            Where-Object { $_.DisplayName -match "Windows Software Development Kit" } |
            Sort-Object DisplayVersion -Descending | Select-Object -First 1
        if (-not $entry -or -not $entry.BundleCachePath) { throw "no Windows SDK bundle in the registry" }
        Start-Process -FilePath $entry.BundleCachePath -ArgumentList "/features", "OptionId.WindowsDesktopDebuggers", "/q", "/norestart" -Wait
    '
}

if ! cdb=$(find_cdb); then
    echo "cdb.exe not found; installing the debugging tools from the SDK bundle"
    install_debuggers
    cdb=$(find_cdb)
fi
echo "cdb: $cdb"

symbols_zip="$scratch/OpenSSH-Win64_Symbols.zip"
curl -X GET -fsSL -o "$symbols_zip" "$SYMBOLS_URL"
got=$(sha256sum "$symbols_zip" | cut -d' ' -f1)
if [[ "$got" != "$SYMBOLS_SHA256" ]]; then
    echo "symbols zip sha256 is $got, expected $SYMBOLS_SHA256"
    exit 1
fi
mkdir -p "$scratch/symbols"
unzip -q -o "$symbols_zip" -d "$scratch/symbols"
rm "$symbols_zip"

# Every directory that holds a pdb, then the public symbol server (cached in the scratch directory).
sympath=''
while IFS= read -r -d '' pdb_dir; do
    sympath+="$(cygpath -w "$pdb_dir");"
done < <(find "$scratch/symbols" -name '*.pdb' -printf '%h\0' | sort -zu)
sympath+="srv*$(cygpath -w "$scratch/symcache")*https://msdl.microsoft.com/download/symbols"
echo "symbol path: $sympath"

run_cdb() {
    local dump=$1 commands=$2 result=$3 status=0
    "$cdb" -z "$(cygpath -w "$dump")" -y "$sympath" -cf "$(cygpath -w "$commands")" > "$result" 2>&1 || status=$?
    if [[ "$status" -ne 0 ]]; then
        echo "cdb exit $status on $dump ($commands)"
    fi
}

count=0
while IFS= read -r -d '' dump; do
    relative=${dump#"$dump_root"/}
    name=${relative//\//_}
    echo "== $relative"
    run_cdb "$dump" "$here/cdb-stacks.txt" "$out_dir/${name%.dmp}.stack.txt"
    run_cdb "$dump" "$here/cdb-fds.txt" "$out_dir/${name%.dmp}.fds.txt"
    count=$((count + 1))
done < <(find "$dump_root" -name '*.dmp' -print0)
echo "dumps read: $count"
