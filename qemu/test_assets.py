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
