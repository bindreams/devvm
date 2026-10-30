"""Tests for assets.py.

Run: uv run --with pytest pytest qemu/test_assets.py
"""

import argparse
import random
import sys
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import assets  # noqa: E402
import third_party  # noqa: E402


def zipped_size(tmp_path: Path, level: int) -> int:
    rng = random.Random(0)
    source = tmp_path / "data"
    source.write_text(" ".join(str(rng.randrange(5000)) for _ in range(200_000)))
    out = tmp_path / f"level{level}.zip"
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=level) as archive:
        assets.add_file(archive, "data", source)
    return out.stat().st_size


def test_add_file_compresses_at_the_level_of_the_archive(tmp_path):
    assert zipped_size(tmp_path, 9) < zipped_size(tmp_path, 1)


def check_manifest(manifest: Path) -> int:
    return assets.cmd_check_manifest(argparse.Namespace(manifest=manifest, repo="o/r"))


def test_a_missing_manifest_passes_while_no_qemu_release_is_published(tmp_path, monkeypatch):
    monkeypatch.setattr(assets, "published_qemu_tags", lambda repo: [])
    assert check_manifest(tmp_path / "qemu-manifest.toml") == 0


def test_a_missing_manifest_fails_once_a_qemu_release_is_published(tmp_path, monkeypatch, capsys):
    monkeypatch.setattr(assets, "published_qemu_tags", lambda repo: ["qemu-11.1.2-1"])
    assert check_manifest(tmp_path / "qemu-manifest.toml") == 1
    assert "qemu-11.1.2-1" in capsys.readouterr().out


# check-urls -----------------------------------------------------------------------------------------------------------

VERSION = third_party.QEMU_VERSION
TAG, REPO = f"qemu-{VERSION}-1", "o/r"
MIRROR = "mingw-w64-foo-1_2.0-1.src.tar.zst"


def release_zip(tmp_path: Path, extra_source_text: str = "") -> Path:
    """A zip holding the THIRD-PARTY.txt and SOURCE.txt that the build writes."""
    stage = tmp_path / "stage"
    stage.mkdir()
    foo = third_party.Package("mingw-w64-ucrt-x86_64-foo", "1:2.0-1", "spdx:LGPL-2.1-or-later", "copyleft", "mingw-w64-foo")
    third_party.write_third_party(stage, [foo], TAG, REPO, [])
    third_party.write_source_offer(stage, [foo], TAG, REPO)
    with (stage / "SOURCE.txt").open("a", encoding="utf-8", newline="\n") as f:
        f.write(extra_source_text)
    out = tmp_path / f"{TAG}-x86_64-pc-windows-msvc.zip"
    with zipfile.ZipFile(out, "w") as archive:
        for name in ("THIRD-PARTY.txt", "SOURCE.txt"):
            archive.write(stage / name, name)
    return out


def release_assets(zip_path: Path) -> list[str]:
    return [
        zip_path.name,
        f"{zip_path.name}.sha256",
        f"qemu-{VERSION}.tar.xz",
        f"qemu-{VERSION}.tar.xz.sig",
        f"{TAG}-qapi.zip",
        f"{TAG}-build-scripts.tar.gz",
        "qemu-manifest.toml",
        MIRROR,
    ]


def check_urls(zip_path: Path, names: list[str]) -> int:
    return assets.cmd_check_urls(argparse.Namespace(zip=zip_path, tag=TAG, repo=REPO, assets=names))


def test_the_urls_a_build_writes_into_the_zip_are_all_accepted(tmp_path, capsys):
    zip_path = release_zip(tmp_path)
    assert check_urls(zip_path, release_assets(zip_path)) == 0, capsys.readouterr().out
    # SOURCE.txt names the release page itself, not an asset.
    assert f"https://github.com/{REPO}/releases/tag/{TAG}" in capsys.readouterr().out


def test_a_url_naming_an_asset_the_release_lacks_is_rejected(tmp_path, capsys):
    zip_path = release_zip(tmp_path)
    names = [name for name in release_assets(zip_path) if name != MIRROR]
    assert check_urls(zip_path, names) == 1
    assert MIRROR in capsys.readouterr().out


def test_a_release_page_of_another_tag_is_rejected(tmp_path, capsys):
    other = f"https://github.com/{REPO}/releases/tag/qemu-9.9.9-9"
    zip_path = release_zip(tmp_path, f"\nSee also {other}\n")
    assert check_urls(zip_path, release_assets(zip_path)) == 1
    assert other in capsys.readouterr().out


# notes ----------------------------------------------------------------------------------------------------------------


def release_notes(capsys) -> str:
    assets.cmd_notes(argparse.Namespace(tag=TAG, repo=REPO))
    return capsys.readouterr().out


def test_the_notes_say_the_build_applies_patches_to_the_tarball(capsys):
    notes = release_notes(capsys)
    assert "qemu/patches" in notes
    assert "exact upstream source the build was made from" not in notes
