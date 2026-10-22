#!/usr/bin/env bash
# Build the pinned QEMU (qemu/pin.env) for Windows x64 in an MSYS2 UCRT64 shell.
#
# Input env:  STAGE  output directory (receives bin/, share/, licenses/qemu/, BUILD-INFO.txt)
#             WORK   scratch directory (receives the verified tarball, signature and source tree)
# DLLs are bundled afterwards by pe_deps.py; THIRD-PARTY.txt and SOURCE.txt by third_party.py.
set -Eeuxo pipefail
exec 2>&1

stage_name="start"
trap 'echo "build.sh: stage \"${stage_name}\" failed at line ${LINENO} (exit $?)" >&2' ERR

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

stage() {
    stage_name="$1"
    echo "=== stage ${stage_name} ==="
}

fail() {
    echo "build.sh: stage \"${stage_name}\": $*" >&2
    exit 1
}

load_pin() {
    # shellcheck source=pin.env
    source "$here/pin.env"
    : "${QEMU_VERSION:?}" "${QEMU_REVISION:?}" "${QEMU_SHA256:?}" "${QEMU_SIGNING_KEY:?}"
    : "${STAGE:?STAGE must name the output directory}" "${WORK:?WORK must name a scratch directory}"
    STAGE="$(cygpath -u "$STAGE")"
    WORK="$(cygpath -u "$WORK")"
    tarball="qemu-${QEMU_VERSION}.tar.xz"
    tarball_url="https://download.qemu.org/${tarball}"
    src="$WORK/src/qemu-${QEMU_VERSION}"
    mkdir -p "$WORK"
}

download() {
    stage "a: download"
    curl -fsSL -o "$WORK/$tarball" "$tarball_url"
    curl -fsSL -o "$WORK/$tarball.sig" "$tarball_url.sig"
}

verify_sha256() {
    stage "b: sha256"
    local actual
    actual="$(sha256sum "$WORK/$tarball")"
    actual="${actual%% *}"
    if [[ "$actual" != "$QEMU_SHA256" ]]; then
        fail "sha256 mismatch for ${tarball_url}: expected ${QEMU_SHA256}, got ${actual}"
    fi
}

verify_signature() {
    stage "c: gpg"
    local gnupg_home="$WORK/gnupg" status valid primary
    rm -rf "$gnupg_home"
    mkdir -p "$gnupg_home"
    chmod 700 "$gnupg_home"

    curl -fsSL -o "$WORK/signer.asc" "https://keys.openpgp.org/vks/v1/by-fingerprint/${QEMU_SIGNING_KEY}"
    gpg --homedir "$gnupg_home" --batch --import "$WORK/signer.asc"

    if ! status="$(gpg --homedir "$gnupg_home" --batch --status-fd 1 --verify "$WORK/$tarball.sig" "$WORK/$tarball" 2>"$WORK/gpg-verify.err")"; then
        cat "$WORK/gpg-verify.err"
        fail "gpg --verify failed for ${tarball}"
    fi

    # An expired or revoked key still reports a signature as valid on stderr; the status lines say which it is.
    if grep -Eq '^\[GNUPG:\] (BADSIG|EXPKEYSIG|REVKEYSIG|EXPSIG|ERRSIG) ' <<<"$status"; then
        fail "bad, expired or revoked signature: $(grep -E '^\[GNUPG:\] (BADSIG|EXPKEYSIG|REVKEYSIG|EXPSIG|ERRSIG) ' <<<"$status")"
    fi
    if ! grep -q '^\[GNUPG:\] GOODSIG ' <<<"$status"; then
        fail "no GOODSIG line in: ${status}"
    fi

    # VALIDSIG <fpr> <date> <ts> <expire> <version> <reserved> <pk-algo> <hash-algo> <class> <primary-key-fpr>
    valid="$(grep '^\[GNUPG:\] VALIDSIG ' <<<"$status" || true)"
    primary="${valid##* }"
    if [[ -z "$valid" || "$primary" != "$QEMU_SIGNING_KEY" ]]; then
        fail "signature is not from ${QEMU_SIGNING_KEY} (VALIDSIG: ${valid:-none})"
    fi
}

extract() {
    stage "d: extract"
    # The tarball holds 72 symlinks, none read by this build, and an MSYS2 tar cannot create them as native links.
    # Skip exactly those (and only those, so a truncated extract still fails) and record them.
    rm -rf "$WORK/src"
    mkdir -p "$WORK/src"
    python - "$WORK/$tarball" "$WORK/symlinks.txt" <<'PY'
import sys
import tarfile

tarball, out = sys.argv[1:]
skippable = ("roms/", "tests/", "rust/", "subprojects/libvduse/", "subprojects/libvhost-user/")
links = []
with tarfile.open(tarball) as archive:
    for member in archive:
        if member.issym() or member.islnk():
            if not member.name.split("/", 1)[1].startswith(skippable):
                sys.exit(f"link outside the paths this build can skip: {member.name}")
            links.append(member.name)
with open(out, "w", newline="\n") as f:
    f.write("\n".join(links) + "\n")
print(f"{len(links)} links to skip")
PY
    tar -xf "$WORK/$tarball" -C "$WORK/src" --anchored --no-wildcards --exclude-from="$WORK/symlinks.txt"
    if [[ ! -f "$src/configure" ]]; then
        fail "no configure script in ${src}"
    fi
}

embed_manifest() {
    stage "e: manifest"
    # version.rc is linked into every QEMU executable (meson.build: version_res), so one resource line covers them
    # all. 24 is RT_MANIFEST and 1 the id Windows reads for an executable. A silent no-op would give every executable
    # the ANSI code page again, so check the result.
    local line='1 24 "devvm.manifest"'
    cp "$here/devvm.manifest" "$src/devvm.manifest"
    printf '
%s
' "$line" >>"$src/version.rc"
    if ! grep -qxF "$line" "$src/version.rc"; then
        fail "version.rc lacks the manifest resource line"
    fi
    if ! cmp -s "$here/devvm.manifest" "$src/devvm.manifest"; then
        fail "devvm.manifest was not copied into the source root"
    fi
}

configure() {
    stage "f: configure"
    # Everything off, then only what devvm needs; each --enable-* fails configure when its dependency is missing.
    # pixman: the VGA and virtio-gpu devices need it. --disable-debug-info: size. The prefix has a drive letter so
    # MSYS2 argument conversion leaves it alone; the install is relocated into $STAGE afterwards.
    configure_args=(
        --without-default-features
        --enable-system --enable-tcg --enable-whpx --enable-slirp --enable-tools --enable-pixman
        --enable-fdt=system --enable-install-blobs
        --disable-docs --disable-guest-agent --disable-rust --disable-download --disable-debug-info
        --target-list=x86_64-softmmu,aarch64-softmmu
        --prefix=C:/qemu --bindir=bin --datadir=share
    )
    rm -rf "$WORK/build"
    mkdir -p "$WORK/build"
    (cd "$WORK/build" && "$src/configure" "${configure_args[@]}")
}

compile() {
    stage "g: make"
    (cd "$WORK/build" && make -j"$(nproc)")
}

install_to_destdir() {
    stage "h: install"
    rm -rf "$WORK/destdir"
    (cd "$WORK/build" && make DESTDIR="$(cygpath -m "$WORK/destdir")" install)
}

lay_out_stage() {
    stage "i: layout"
    local -a hits
    local root path
    mapfile -d '' -t hits < <(find "$WORK/destdir" -type f -name qemu-system-x86_64.exe -path '*/bin/*' -print0)
    if [[ "${#hits[@]}" -ne 1 ]]; then
        fail "expected one bin/qemu-system-x86_64.exe under ${WORK}/destdir, found ${#hits[@]}"
    fi
    root="$(dirname "$(dirname "${hits[0]}")")"
    if [[ ! -d "$root/share" ]]; then
        fail "no share beside ${hits[0]}"
    fi

    rm -rf "$STAGE"
    mkdir -p "$STAGE"
    mv "$root/bin" "$root/share" "$STAGE/"

    for path in bin/qemu-img.exe bin/qemu-system-aarch64.exe bin/qemu-system-x86_64.exe \
        share/edk2-x86_64-code.fd share/edk2-i386-vars.fd \
        share/edk2-aarch64-code.fd share/edk2-arm-vars.fd \
        share/vgabios-stdvga.bin share/efi-e1000e.rom share/efi-virtio.rom share/keymaps/en-us; do
        if [[ ! -f "$STAGE/$path" ]]; then
            fail "installed tree lacks ${path}"
        fi
    done
}

prune() {
    stage "j: prune"
    local tool found expected="qemu-img.exe qemu-system-aarch64.exe qemu-system-x86_64.exe"
    # Firmware descriptors hold install-time absolute paths; devvm passes explicit pflash paths.
    rm -rf "$STAGE/share/firmware"
    # --enable-tools builds these together with qemu-img; devvm uses none of them.
    for tool in qemu-io qemu-nbd qemu-storage-daemon qemu-edid; do
        rm -f "$STAGE/bin/$tool.exe"
    done

    found="$(cd "$STAGE/bin" && ls -1 -- *.exe | tr '\n' ' ')"
    found="${found% }"
    if [[ "$found" != "$expected" ]]; then
        fail "executables in bin/ are '${found}', expected '${expected}'"
    fi
}

copy_licenses() {
    stage "k: licenses"
    mkdir -p "$STAGE/licenses/qemu"
    cp "$src/COPYING" "$src/COPYING.LIB" "$src/LICENSE" "$src/pc-bios/edk2-licenses.txt" "$STAGE/licenses/qemu/"
}

write_build_info() {
    stage "l: build info"
    {
        echo "QEMU version: ${QEMU_VERSION}"
        echo "Release revision: ${QEMU_REVISION}"
        echo "Source tarball: ${tarball_url}"
        echo "Source sha256: ${QEMU_SHA256}"
        echo "Signing key: ${QEMU_SIGNING_KEY}"
        echo "Build scripts commit: ${GITHUB_SHA:-unknown}"
        echo "Configure: ${configure_args[*]}"
        echo
        echo "Symlinks of the tarball not extracted (not read by this build):"
        cat "$WORK/symlinks.txt"
        echo
        echo "Packages of the build environment (pacman -Q):"
        pacman -Q
    } >"$STAGE/BUILD-INFO.txt"
}

main() {
    load_pin
    download
    verify_sha256
    verify_signature
    extract
    embed_manifest
    configure
    compile
    install_to_destdir
    lay_out_stage
    prune
    copy_licenses
    write_build_info
    stage "done"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    main
fi
