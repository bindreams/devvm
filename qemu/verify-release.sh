#!/usr/bin/env bash
# The QEMU track's own checks on a release's downloaded assets (see reusable-verify-release.yaml).
#
# Usage: verify-release.sh <assets dir>
# Input env: TAG  the release tag (qemu-<version>-<revision>)
#            GITHUB_REPOSITORY  owner/repo
# Run from the repository root at the tagged commit.
set -Eeuo pipefail
shopt -s nullglob

trap 'echo "verify-release.sh: failed at line ${LINENO} (exit $?)" >&2' ERR

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

main() {
    local assets="${1:?usage: verify-release.sh <assets dir>}"
    : "${TAG:?}" "${GITHUB_REPOSITORY:?}"

    # shellcheck source=pin.env
    source "$here/pin.env"

    # The tag names the pin it was built from; the checks below read that pin.
    if [[ "$TAG" != "qemu-${QEMU_VERSION}-${QEMU_REVISION}" ]]; then
        echo "::error::the tag ${TAG} is not the pin at its commit: qemu-${QEMU_VERSION}-${QEMU_REVISION}"
        exit 1
    fi

    check_tarball "$assets"
    check_urls "$assets"
}

# The attached source tarball is the pinned one.
check_tarball() {
    local assets="$1" actual
    actual="$(sha256sum < "${assets}/qemu-${QEMU_VERSION}.tar.xz")"
    actual="${actual%% *}"
    if [[ "$actual" != "$QEMU_SHA256" ]]; then
        echo "::error::the published QEMU tarball is not the pinned one: ${actual} != ${QEMU_SHA256}"
        exit 1
    fi
}

# Every URL the zip's THIRD-PARTY.txt and SOURCE.txt name, and every fixed asset of the release, must be an asset of
# this release (or the release page).
check_urls() {
    local assets="$1" file zip="" names=()
    for file in "${assets}/${TAG}-"*.zip; do
        if [[ "$file" != "${assets}/${TAG}-qapi.zip" ]]; then
            if [[ -n "$zip" ]]; then
                echo "::error::more than one build zip: ${zip##*/}, ${file##*/}"
                exit 1
            fi
            zip="$file"
        fi
    done
    if [[ -z "$zip" ]]; then
        echo "::error::no build zip ${TAG}-<triple>.zip in the release"
        exit 1
    fi
    for file in "${assets}"/*; do
        names+=("${file##*/}")
    done
    uv run --script "$here/assets.py" check-urls --zip "$zip" --tag "$TAG" --repo "$GITHUB_REPOSITORY" --assets "${names[@]}"
}

main "$@"
