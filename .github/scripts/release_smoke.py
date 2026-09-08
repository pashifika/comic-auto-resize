#!/usr/bin/env python3
"""Exercise a packaged comic-auto-resize binary the way a user would run it.

A `--version` call proves the file is not a stub and nothing else. What ships here is a
converter, so the smoke test converts: it generates a redistributable comic archive, runs
the real executable against a copy, and checks the pages that came out and the input that
went in. The fixture is generated rather than taken from `samples/`, which holds material
this project does not redistribute. A second generated fixture is one asymmetric spread, so
the opt-in split, its trim and offset geometry and both reading orders are exercised by the
same packaged and downloaded executables.
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
# One landscape page inside the tool's 1.05-1.60 split gate: 2800/2000 is 1.40.
SPREAD_WIDTH = 2800
SPREAD_HEIGHT = 2000
# Halve the spread, then trim the gutter and shift both windows right.
SPLIT_PERCENT = 50
TRIM_PERCENT = 48
TRIM_OFFSET = 20
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
    digest: str


def half_up(numerator: int, denominator: int) -> int:
    """Divide with half-up rounding, the rule the binary's geometry uses."""

    return (numerator * 2 + denominator) // (denominator * 2)


def window_width(percent: int) -> int:
    """Source columns one split window covers on the generated spread."""

    return half_up(SPREAD_WIDTH * percent, 100)


def normalised_height(source_width: int, source_height: int) -> int:
    """Height a source window has once it is normalised to the default target width."""

    return half_up(source_height * DEFAULT_AUTO_WIDTH, source_width)


def encode_png(width: int, height: int, rows: bytes) -> bytes:
    """Wrap filtered scanlines as an 8-bit truecolour PNG."""

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
        + chunk(b"IDAT", zlib.compress(rows, 6))
        + chunk(b"IEND", b"")
    )


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

    return encode_png(width, height, bytes(rows))


def png_spread(width: int, height: int) -> bytes:
    """Return a landscape page whose two halves are different images.

    The left-to-right ramp on the blue channel is what makes them different. On a
    horizontally symmetric page a reversed reading order would store the same bytes in the
    same places, and an inert `--reading-order` would pass.
    """

    band = max(1, height // 8)
    variants = []
    for dark in (True, False):
        row = bytearray(b"\x00")  # filter type 0, so the bytes below are the pixels
        for x in range(width):
            on = ((x // 16) % 2 == 0) != dark
            value = 24 if on else 232
            row += bytes((value, value, x * 255 // (width - 1)))
        variants.append(bytes(row))
    return encode_png(
        width, height, b"".join(variants[(y // band) % 2] for y in range(height))
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


def write_spread_fixture(path: Path) -> Path:
    """Write one redistributable archive holding a single generated spread."""

    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as archive:
        archive.writestr("spread-001.png", png_spread(SPREAD_WIDTH, SPREAD_HEIGHT))
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
                data = archive.read(info)
                width, height = jpeg_dimensions(data)
                pages.append(
                    Page(info.filename, width, height, hashlib.sha256(data).hexdigest())
                )
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


def convert_spread(
    executable: Path, source: Path, output: Path, *flags: str
) -> list[Page]:
    """Convert the spread fixture to a named output and read its pages back."""

    run_binary(executable, *flags, "--out", str(output), str(source))
    if not output.is_file():
        raise SmokeError(f"conversion wrote no archive at {output}")
    return read_pages(output)


def expect_geometry(
    pages: list[Page], label: str, expected: list[tuple[int, int]]
) -> None:
    """Raise unless the produced pages have exactly this geometry, in stored order."""

    produced = [(page.width, page.height) for page in pages]
    if produced != expected:
        raise SmokeError(f"{label} produced {produced}; expected {expected}")


def smoke_spread(executable: Path, *, work_directory: Path) -> list[str]:
    """Prove one packaged executable splits a spread the way the options promise.

    Returns one summary line per invocation so the run leaves the observed geometry in the
    job log, not only a pass.
    """

    work_directory.mkdir(parents=True, exist_ok=True)
    source = write_spread_fixture(work_directory / "spread-fixture.zip")
    original = work_directory / "spread.zip"
    shutil.copyfile(source, original)
    original_digest = sha256_file(original)

    whole_height = normalised_height(SPREAD_WIDTH, SPREAD_HEIGHT)
    half_height = normalised_height(window_width(SPLIT_PERCENT), SPREAD_HEIGHT)
    trim_height = normalised_height(window_width(TRIM_PERCENT), SPREAD_HEIGHT)

    whole = convert_spread(executable, original, work_directory / "whole.zip")
    expect_geometry(
        whole, "conversion without --split", [(DEFAULT_AUTO_WIDTH, whole_height)]
    )

    halves = convert_spread(
        executable, original, work_directory / "halves.zip", f"--split={SPLIT_PERCENT}"
    )
    expect_geometry(
        halves, f"--split={SPLIT_PERCENT}", [(DEFAULT_AUTO_WIDTH, half_height)] * 2
    )

    untrimmed = convert_spread(
        executable, original, work_directory / "untrimmed.zip", f"--split={TRIM_PERCENT}"
    )
    expect_geometry(
        untrimmed, f"--split={TRIM_PERCENT}", [(DEFAULT_AUTO_WIDTH, trim_height)] * 2
    )

    trim = (f"--split={TRIM_PERCENT}", f"--split-pos={TRIM_OFFSET}")
    trimmed = convert_spread(executable, original, work_directory / "trimmed.zip", *trim)
    expect_geometry(trimmed, " ".join(trim), [(DEFAULT_AUTO_WIDTH, trim_height)] * 2)
    if [page.digest for page in trimmed] == [page.digest for page in untrimmed]:
        raise SmokeError(
            f"--split-pos={TRIM_OFFSET} produced the unshifted pieces; the offset was ignored"
        )
    if trimmed[0].digest == trimmed[1].digest:
        raise SmokeError(
            "the two trimmed pieces are byte-identical, so reading order proves nothing"
        )

    reversed_pieces = convert_spread(
        executable, original, work_directory / "reversed.zip", *trim, "--reading-order=l"
    )
    expect_geometry(
        reversed_pieces, "--reading-order=l", [(DEFAULT_AUTO_WIDTH, trim_height)] * 2
    )
    if [page.name for page in reversed_pieces] != [page.name for page in trimmed]:
        raise SmokeError("reading order changed the stored page names, not their contents")
    if [page.digest for page in reversed_pieces] != [
        page.digest for page in reversed(trimmed)
    ]:
        raise SmokeError("--reading-order=l did not reverse the stored pieces")

    if not original.is_file() or sha256_file(original) != original_digest:
        raise SmokeError("the input archive was modified or removed by a split conversion")

    return [
        f"no --split: 1 page {DEFAULT_AUTO_WIDTH}x{whole_height}",
        f"--split={SPLIT_PERCENT}: 2 pages {DEFAULT_AUTO_WIDTH}x{half_height}",
        f"--split={TRIM_PERCENT}: 2 pages {DEFAULT_AUTO_WIDTH}x{trim_height}",
        f"--split={TRIM_PERCENT} --split-pos={TRIM_OFFSET}: 2 pages "
        f"{DEFAULT_AUTO_WIDTH}x{trim_height}, shifted",
        "--reading-order=l: the same two names carrying the reversed pieces",
    ]


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
    for option in (
        "--auto-width",
        "--ratio",
        "--out",
        "--quality",
        "--split",
        "--split-pos",
        "--reading-order",
    ):
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

    for line in smoke_spread(executable, work_directory=work_directory / "spread"):
        print(f"  {line}")
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
