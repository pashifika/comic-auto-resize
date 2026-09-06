#!/usr/bin/env python3
"""Exercise a packaged comic-auto-resize binary the way a user would run it.

A `--version` call proves the file is not a stub and nothing else. What ships here is a
converter, so the smoke test converts: it generates a redistributable comic archive, runs
the real executable against a copy, and checks the pages that came out and the input that
went in. The fixture is generated rather than taken from `samples/`, which holds material
this project does not redistribute.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import stat
import struct
import subprocess
import sys
import tarfile
import zipfile
import zlib
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

import release as release_assets

PRODUCT_NAME = release_assets.PRODUCT_NAME
FIXTURE_PAGES = 3
FIXTURE_WIDTH = 1400
FIXTURE_HEIGHT = 2000
# The binary's default target width. A fixture narrower than this would be left alone, and
# a smoke test that proves nothing was resized proves nothing about resizing.
DEFAULT_AUTO_WIDTH = 1280
# JPEG start-of-frame markers, less the three `FFC*` codes that are not frame headers:
# `C4` DHT, `C8` JPG, `CC` DAC.
SOF_MARKERS = frozenset(
    {0xC0, 0xC1, 0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF}
)


class SmokeError(RuntimeError):
    """A packaged binary did not behave the way a download promises."""


@dataclass(frozen=True)
class Page:
    """One decoded page property set read back out of a produced archive."""

    name: str
    width: int
    height: int


def png_page(width: int, height: int, index: int) -> bytes:
    """Return a deterministic RGB PNG standing in for one comic page."""

    rows = bytearray()
    band = max(1, height // 8)
    for y in range(height):
        rows.append(0)  # filter type 0, so the bytes below are the pixels
        dark = (y // band + index) % 2 == 0
        row = bytearray()
        for x in range(width):
            # Vertical bars over alternating bands: high-contrast edges, which is what a
            # resampler is actually asked to preserve here.
            on = ((x // 16) % 2 == 0) != dark
            value = 24 if on else 232
            row += bytes((value, value, min(255, value + index * 8)))
        rows += row

    def chunk(kind: bytes, payload: bytes) -> bytes:
        return (
            struct.pack(">I", len(payload))
            + kind
            + payload
            + struct.pack(">I", zlib.crc32(kind + payload) & 0xFFFFFFFF)
        )

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(bytes(rows), 6))
        + chunk(b"IEND", b"")
    )


def write_fixture(path: Path, *, pages: int = FIXTURE_PAGES) -> Path:
    """Write one redistributable comic archive of generated pages."""

    if pages < 1:
        raise SmokeError("a comic fixture needs at least one page")
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as archive:
        for index in range(pages):
            archive.writestr(
                f"page-{index + 1:03d}.png",
                png_page(FIXTURE_WIDTH, FIXTURE_HEIGHT, index),
            )
    return path


def jpeg_dimensions(data: bytes) -> tuple[int, int]:
    """Return one JPEG's frame dimensions, or raise when it is not a JPEG."""

    if len(data) < 4 or data[:3] != b"\xff\xd8\xff":
        raise SmokeError("page is not a JPEG image")
    offset = 2
    while offset + 4 <= len(data):
        if data[offset] != 0xFF:
            raise SmokeError("JPEG marker segment is misaligned")
        marker = data[offset + 1]
        if marker in (0xD8, 0x01) or 0xD0 <= marker <= 0xD7:
            offset += 2
            continue
        (length,) = struct.unpack_from(">H", data, offset + 2)
        if length < 2 or offset + 2 + length > len(data):
            raise SmokeError("JPEG marker segment is truncated")
        if marker in SOF_MARKERS:
            _, height, width = struct.unpack_from(">BHH", data, offset + 4)
            return width, height
        offset += 2 + length
    raise SmokeError("JPEG carries no start-of-frame header")


def read_pages(archive_path: Path) -> list[Page]:
    """Return the pages of a produced archive in stored order."""

    try:
        with zipfile.ZipFile(archive_path) as archive:
            pages = []
            for info in archive.infolist():
                if info.is_dir():
                    raise SmokeError(f"output archive holds a directory: {info.filename}")
                width, height = jpeg_dimensions(archive.read(info))
                pages.append(Page(info.filename, width, height))
            return pages
    except (OSError, zipfile.BadZipFile) as error:
        raise SmokeError(f"cannot read produced archive {archive_path}: {error}") from error


def run_binary(executable: Path, *arguments: str) -> subprocess.CompletedProcess[str]:
    """Run the packaged executable and return decoded output."""

    completed = subprocess.run(
        (str(executable), *arguments),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    if completed.returncode != 0:
        raise SmokeError(
            f"{executable.name} {' '.join(arguments)} failed with status "
            f"{completed.returncode}: {completed.stderr.strip()}"
        )
    return completed


def sha256_file(path: Path) -> str:
    """Return a SHA-256 digest for one file."""

    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def smoke_binary(executable: Path, *, version: str, work_directory: Path) -> list[Page]:
    """Prove one packaged executable identifies itself and converts a comic archive."""

    release_assets.validate_stable_version(version)
    release_assets.require_regular_file(executable, "packaged executable")
    if os.name != "nt" and not executable.stat().st_mode & stat.S_IXUSR:
        raise SmokeError(f"packaged executable is not executable: {executable}")

    reported = run_binary(executable, "--version").stdout.strip()
    if reported != f"{PRODUCT_NAME} {version}":
        raise SmokeError(
            f"--version reported {reported!r}; expected {PRODUCT_NAME} {version}"
        )
    help_text = run_binary(executable, "--help").stdout
    for option in ("--auto-width", "--ratio", "--out", "--quality"):
        if option not in help_text:
            raise SmokeError(f"--help does not document {option}")

    work_directory.mkdir(parents=True, exist_ok=True)
    source = write_fixture(work_directory / "fixture.zip")
    # Converted through a copy, exactly as the README instructs, so the assertion below is
    # about the tool's promise to keep an original rather than about this script's care.
    original = work_directory / "volume.zip"
    shutil.copyfile(source, original)
    original_digest = sha256_file(original)

    completed = run_binary(executable, str(original))
    produced = work_directory / "volume_resize.zip"
    if not produced.is_file():
        raise SmokeError(f"conversion wrote no archive at {produced}")
    if f"{FIXTURE_PAGES} page(s) written" not in completed.stdout:
        raise SmokeError(
            f"conversion reported {completed.stdout.strip()!r}, not "
            f"{FIXTURE_PAGES} written pages"
        )

    pages = read_pages(produced)
    if len(pages) != FIXTURE_PAGES:
        raise SmokeError(
            f"produced archive holds {len(pages)} page(s); expected {FIXTURE_PAGES}"
        )
    expected_height = round(FIXTURE_HEIGHT * DEFAULT_AUTO_WIDTH / FIXTURE_WIDTH)
    for page in pages:
        if page.width != DEFAULT_AUTO_WIDTH:
            raise SmokeError(
                f"page {page.name} is {page.width}px wide; expected {DEFAULT_AUTO_WIDTH}"
            )
        if abs(page.height - expected_height) > 1:
            raise SmokeError(
                f"page {page.name} is {page.height}px tall; expected about {expected_height}"
            )

    if not original.is_file() or sha256_file(original) != original_digest:
        raise SmokeError("the input archive was modified or removed by a plain conversion")
    return pages


def extract_package(
    archive: Path, target: release_assets.Target, tag: str, destination: Path
) -> Path:
    """Extract one release archive and return the extracted executable path."""

    destination.mkdir(parents=True, exist_ok=True)
    root = release_assets.asset_stem(tag, target)
    if target.extension == ".zip":
        with zipfile.ZipFile(archive) as package:
            for info in package.infolist():
                release_assets.validate_member_name(info.filename)
                package.extract(info, destination)
                if info.is_dir():
                    continue
                # A ZIP round trip drops the mode on extraction, so it is restored from the
                # member metadata the package contract already asserted.
                mode = (info.external_attr >> 16) & 0o777
                if mode and os.name != "nt":
                    (destination / info.filename).chmod(mode)
    else:
        with tarfile.open(archive, mode="r:gz") as package:
            package.extractall(destination, filter="data")

    extracted = destination / root / release_assets.executable_name(target)
    release_assets.require_regular_file(extracted, "extracted executable")
    if target.triple == "aarch64-apple-darwin" and not extracted.stat().st_mode & 0o111:
        raise SmokeError(f"extraction did not preserve the executable bit: {extracted}")
    for name in release_assets.expected_file_names(target):
        release_assets.require_regular_file(destination / root / name, name)
    return extracted


def verify_download(
    directory: Path,
    work_directory: Path,
    *,
    target: release_assets.Target,
    version: str,
    tag: str,
    commit: str,
) -> list[Page]:
    """Verify downloaded public release assets and run the extracted program."""

    release_assets.verify_release_set(
        directory, version=version, tag=tag, commit=commit
    )
    archive = directory / release_assets.asset_name(tag, target)
    extracted = extract_package(archive, target, tag, work_directory / "extracted")
    metadata = (
        (work_directory / "extracted" / release_assets.asset_stem(tag, target))
        / release_assets.VERSION_FILE
    ).read_bytes()
    expected = release_assets.version_metadata(version, tag, target, commit)
    if metadata != expected:
        raise SmokeError("extracted VERSION metadata does not match the validated release")
    release_assets.inspect_linkage(extracted, target)
    return smoke_binary(
        extracted, version=version, work_directory=work_directory / "run"
    )


def argument_parser() -> argparse.ArgumentParser:
    """Build the smoke-helper command-line parser."""

    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    fixture_parser = subparsers.add_parser("fixture")
    fixture_parser.add_argument("--output", type=Path, required=True)
    fixture_parser.add_argument("--pages", type=int, default=FIXTURE_PAGES)

    smoke_parser = subparsers.add_parser("smoke")
    smoke_parser.add_argument("--executable", type=Path, required=True)
    smoke_parser.add_argument("--version", required=True)
    smoke_parser.add_argument("--work-directory", type=Path, required=True)

    download_parser = subparsers.add_parser("verify-download")
    download_parser.add_argument("--directory", type=Path, required=True)
    download_parser.add_argument("--work-directory", type=Path, required=True)
    download_parser.add_argument("--target", required=True)
    download_parser.add_argument("--version", required=True)
    download_parser.add_argument("--tag", required=True)
    download_parser.add_argument("--commit", required=True)

    return parser


def run(arguments: Sequence[str]) -> int:
    """Run one smoke-helper subcommand."""

    options = argument_parser().parse_args(arguments)

    if options.command == "fixture":
        path = write_fixture(options.output, pages=options.pages)
        print(f"Wrote {path} ({path.stat().st_size} bytes)")
        return 0

    if options.command == "smoke":
        pages = smoke_binary(
            options.executable,
            version=options.version,
            work_directory=options.work_directory,
        )
        print(f"Converted {len(pages)} page(s) with {options.executable}")
        for page in pages:
            print(f"  {page.name}: {page.width}x{page.height}")
        return 0

    if options.command == "verify-download":
        target = release_assets.target_for(options.target)
        pages = verify_download(
            options.directory,
            options.work_directory,
            target=target,
            version=options.version,
            tag=options.tag,
            commit=options.commit,
        )
        print(
            f"Downloaded release {options.tag} verified and executed on {target.triple}; "
            f"{len(pages)} page(s) converted"
        )
        return 0

    raise SmokeError(f"unhandled smoke command {options.command!r}")


def main(arguments: Sequence[str] | None = None) -> int:
    """Convert smoke failures into a stable nonzero status."""

    try:
        return run(sys.argv[1:] if arguments is None else arguments)
    except (OSError, SmokeError, release_assets.ReleaseError) as error:
        print(f"release smoke failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
