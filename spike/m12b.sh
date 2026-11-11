#!/usr/bin/env bash
# Spike only (never merged). M12b: can the sidecar's qemu-img write the no-prompt boot image into a qcow2 overlay over the
# raw ISO, with a path holding a space, Cyrillic and a surrogate pair, and does a guest boot from that overlay?
#
# Usage: m12b.sh <qemu-dir> <iso> <seed_spike.exe> <out-dir>
set -euxo pipefail
exec 2>&1

qemu_dir=$1
iso=$2
seed_spike=$3
out=$4

qemu_img=$(find "$qemu_dir" -name qemu-img.exe | head -n 1)
# Outside $out, which is uploaded: the artifact service need not accept this name.
dir="$(dirname "$out")/пробел space 😀 dir"
mkdir -p "$dir"

sha_before=$(sha256sum "$iso" | cut -d' ' -f1)

uv run --script spike/patch_iso.py --extent "$iso" "$dir/noprompt.bin" | tee "$out/extent.txt"
offset=$(sed -n 's/^EXTENT .* offset=\([0-9]*\) .*/\1/p' "$out/extent.txt")
length=$(sed -n 's/^EXTENT .* length=\([0-9]*\)$/\1/p' "$out/extent.txt")
sector=$(sed -n 's/^EXTENT sector=\([0-9]*\) .*/\1/p' "$out/extent.txt")

overlay="$dir/overlay.qcow2"
"$qemu_img" create -f qcow2 -b "$(cygpath -m "$iso")" -F raw "$overlay"

"$qemu_img" convert -n -f raw --target-image-opts "$dir/noprompt.bin" \
    "driver=raw,offset=$offset,size=$length,file.driver=qcow2,file.file.driver=file,file.file.filename=$(cygpath -m "$overlay")"

# Read back independently of the write: the whole overlay as a raw image, then the extent out of it with cmp. (qemu-img dd
# returned a short read here, so it is not used.)
"$qemu_img" convert -f qcow2 -O raw "$overlay" "$dir/full.raw"
ls -l "$dir/full.raw" "$iso"
cmp -i "$offset:0" -n "$length" "$dir/full.raw" "$dir/noprompt.bin"
cmp -n "$offset" "$dir/full.raw" "$iso"
rm "$dir/full.raw"
echo "M12B-WRITE=ok: the extent reads back as efisys_noprompt.bin and the bytes before it equal the ISO's"

# The overlay differs from the ISO only inside the extent; qemu-img compare names the first difference.
if "$qemu_img" compare -f raw -F qcow2 "$iso" "$overlay"; then
    echo "M12B-COMPARE=identical (unexpected)"
else
    echo "M12B-COMPARE=differs, as expected (exit $?)"
fi
"$qemu_img" info "$overlay"
ls -l "$overlay"

sha_after=$(sha256sum "$iso" | cut -d' ' -f1)
test "$sha_before" = "$sha_after"
echo "M12B-ISO-UNTOUCHED=ok"

# Boot from the overlay: Setup reads Autounattend.xml and runs `wpeutil shutdown`; QEMU exiting with 0 is the proof.
"$seed_spike" "$dir/seed.img" spike/m12b/Autounattend.xml
"$qemu_img" create -f qcow2 "$dir/disk.qcow2" 8G
share=$(dirname "$(find "$qemu_dir" -name edk2-x86_64-code.fd | head -n 1)")
cp "$share/edk2-i386-vars.fd" "$dir/vars.fd"
uv run --script spike/run_qemu.py install --single \
    --qemu-dir "$qemu_dir" --out "$out/boot" --disk "$dir/disk.qcow2" --vars "$dir/vars.fd" \
    --install-iso "$overlay" --install-format qcow2 --usb-disk "$dir/seed.img"
grep -F 'guest-shutdown' "$out/boot/launch-1/qmp-events.jsonl"
echo "M12B-BOOT=ok: the guest shut itself down (wpeutil shutdown from Autounattend.xml) after booting from the overlay"
