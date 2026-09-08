#!/usr/bin/env python3
"""Self-tests for the packaged-binary smoke harness.

The harness is the only thing standing between "the file downloaded" and "the program
works", and a harness that stopped asserting would pass on a stub. These cases drive it
against a scripted stand-in whose behaviour can be made wrong on purpose: a wrong version
banner, an unresized page, a missing output, a consumed input.
"""

from __future__ import annotations

import os
import shutil
import struct
import tempfile
import unittest
import zipfile
import zlib
from pathlib import Path

import release as release_assets
from release_smoke import (
    DEFAULT_AUTO_WIDTH,
    FIXTURE_HEIGHT,
    FIXTURE_PAGES,
    FIXTURE_WIDTH,
    SPREAD_HEIGHT,
    SPREAD_WIDTH,
    SmokeError,
    extract_package,
    jpeg_dimensions,
    png_page,
    png_spread,
    read_pages,
    smoke_binary,
    smoke_spread,
    write_fixture,
    write_spread_fixture,
)
from test_release import COMMIT, TAG, VERSION, macho, portable_executable

POSIX_ONLY = unittest.skipIf(
    os.name == "nt", "the scripted stand-in is executed through a shebang"
)


def jpeg(width: int, height: int, *, marker: int = 0xC2) -> bytes:
    """Return the smallest byte string that reads back as a JPEG of that size."""

    application = b"JFIF\0\x01\x02\x00\x00\x01\x00\x01\x00\x00"
    frame = struct.pack(">BHHBB", 8, height, width, 1, 0)
    return (
        b"\xff\xd8\xff\xe0"
        + struct.pack(">H", len(application) + 2)
        + application
        + bytes((0xFF, marker))
        + struct.pack(">H", len(frame) + 2)
        + frame
        + b"\xff\xd9"
    )


STAND_IN = '''#!/usr/bin/env python3
"""A scripted stand-in for the packaged binary, wrong in exactly one way per case."""

import struct
import sys
import zipfile

VERSION = {version!r}
HELP = "--auto-width --ratio --out --quality --split --split-pos --reading-order"
PAGES = {pages}
WIDTH = {width}
HEIGHT = {height}
MODE = {mode!r}
SPLIT_MODE = {split_mode!r}
SPREAD_WIDTH = {spread_width}
TARGET = {target}


def jpeg(width, height, label):
    comment = label.encode()
    application = b"JFIF\\0\\x01\\x02\\x00\\x00\\x01\\x00\\x01\\x00\\x00"
    frame = struct.pack(">BHHBB", 8, height, width, 1, 0)
    return (
        b"\\xff\\xd8\\xff\\xe0"
        + struct.pack(">H", len(application) + 2)
        + application
        + b"\\xff\\xfe"
        + struct.pack(">H", len(comment) + 2)
        + comment
        + b"\\xff\\xc2"
        + struct.pack(">H", len(frame) + 2)
        + frame
        + b"\\xff\\xd9"
    )


def half_up(numerator, denominator):
    return (numerator * 2 + denominator) // (denominator * 2)


options = {{}}
positional = []
arguments = sys.argv[1:]
index = 0
while index < len(arguments):
    item = arguments[index]
    if item == "--out":
        options["--out"] = arguments[index + 1]
        index += 2
        continue
    if item.startswith("--"):
        key, _, value = item.partition("=")
        options[key] = value
    else:
        positional.append(item)
    index += 1

if "--version" in options:
    print(f"comic-auto-resize {{VERSION}}")
    raise SystemExit(0)
if "--help" in options:
    print(HELP)
    raise SystemExit(0)

source = positional[0]
with zipfile.ZipFile(source) as archive:
    first = archive.read(archive.namelist()[0])
source_width, source_height = struct.unpack_from(">II", first, 16)

if MODE == "no-output":
    print(f"{{PAGES}} page(s) written to nowhere")
    raise SystemExit(0)
if MODE == "eat-input":
    open(source, "wb").close()

output = options.get("--out") or source[: -len(".zip")] + "_resize.zip"

if source_width != SPREAD_WIDTH:
    members = [
        (f"page-{{index + 1:03d}}.jpg", jpeg(WIDTH, HEIGHT, "page"))
        for index in range(PAGES)
    ]
elif "--split" not in options or SPLIT_MODE == "whole":
    height = half_up(source_height * TARGET, source_width)
    members = [("spread-001.jpg", jpeg(TARGET, height, "whole"))]
else:
    window = half_up(source_width * int(options["--split"]), 100)
    height = half_up(source_height * TARGET, window)
    if SPLIT_MODE == "geometry":
        height += 40
    offset = int(options.get("--split-pos") or 0)
    order = options.get("--reading-order", "r")
    if SPLIT_MODE == "inert-order":
        order = "r"
    halves = ["right", "left"] if order == "r" else ["left", "right"]
    offsets = [offset, offset]
    if SPLIT_MODE == "inert-offset":
        offsets = [0, 0]
    elif SPLIT_MODE == "one-side-offset":
        offsets = [offset, 0]
    members = [
        (f"spread-001-{{index + 1}}.jpg", jpeg(TARGET, height, f"{{half}}+{{offsets[index]}}"))
        for index, half in enumerate(halves)
    ]

with zipfile.ZipFile(output, "w") as archive:
    for name, data in members:
        archive.writestr(name, data)
print(f"{{len(members)}} page(s) written to {{output}}")
'''


class FixtureTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="smoke-fixture-"))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)

    def test_a_generated_page_is_a_decodable_png_wider_than_the_target(self) -> None:
        """A fixture narrower than the default width would never exercise resizing."""
        data = png_page(FIXTURE_WIDTH, FIXTURE_HEIGHT, 0)
        self.assertEqual(data[:8], b"\x89PNG\r\n\x1a\n")
        self.assertEqual(data[12:16], b"IHDR")
        width, height, depth, colour = struct.unpack_from(">IIBB", data, 16)
        self.assertEqual((width, height, depth, colour), (FIXTURE_WIDTH, FIXTURE_HEIGHT, 8, 2))
        self.assertGreater(FIXTURE_WIDTH, DEFAULT_AUTO_WIDTH)
        start = 16 + 13 + 4 + 8
        pixels = zlib.decompress(data[start : start + struct.unpack_from(">I", data, start - 8)[0]])
        self.assertEqual(len(pixels), FIXTURE_HEIGHT * (1 + FIXTURE_WIDTH * 3))

    def test_a_fixture_is_a_readable_archive_of_pages(self) -> None:
        path = write_fixture(self.root / "fixture.zip", pages=2)
        with zipfile.ZipFile(path) as archive:
            self.assertEqual(archive.namelist(), ["page-001.png", "page-002.png"])

    def test_a_fixture_needs_at_least_one_page(self) -> None:
        with self.assertRaises(SmokeError):
            write_fixture(self.root / "empty.zip", pages=0)

    def test_the_generated_spread_has_two_different_halves(self) -> None:
        """The reversed-order assertion is only meaningful on an asymmetric page."""
        data = png_spread(SPREAD_WIDTH, SPREAD_HEIGHT)
        width, height, depth, colour = struct.unpack_from(">IIBB", data, 16)
        self.assertEqual((width, height, depth, colour), (SPREAD_WIDTH, SPREAD_HEIGHT, 8, 2))
        start = 16 + 13 + 4 + 8
        pixels = zlib.decompress(data[start : start + struct.unpack_from(">I", data, start - 8)[0]])
        stride = 1 + SPREAD_WIDTH * 3
        self.assertEqual(len(pixels), SPREAD_HEIGHT * stride)
        row = pixels[1:stride]
        half = SPREAD_WIDTH // 2 * 3
        self.assertNotEqual(row[:half], row[half:])

    def test_a_spread_fixture_holds_one_page_inside_the_split_gate(self) -> None:
        """Outside 1.05-1.60 the binary refuses to split, and the smoke fails at release."""
        path = write_spread_fixture(self.root / "spread.zip")
        with zipfile.ZipFile(path) as archive:
            self.assertEqual(archive.namelist(), ["spread-001.png"])
        aspect = SPREAD_WIDTH * 100 // SPREAD_HEIGHT
        self.assertGreaterEqual(aspect, 105)
        self.assertLessEqual(aspect, 160)


class JpegReadingTests(unittest.TestCase):
    def test_both_baseline_and_progressive_frames_are_read(self) -> None:
        for marker in (0xC0, 0xC2):
            with self.subTest(marker=marker):
                self.assertEqual(jpeg_dimensions(jpeg(1280, 1829, marker=marker)), (1280, 1829))

    def test_a_non_jpeg_page_is_refused(self) -> None:
        """A conversion that emitted PNG pages would otherwise read as success."""
        for data in (b"", b"\x89PNG\r\n\x1a\n", b"\xff\xd8\xff"):
            with self.subTest(data=data), self.assertRaises(SmokeError):
                jpeg_dimensions(data)

    def test_a_truncated_frame_is_refused(self) -> None:
        with self.assertRaises(SmokeError):
            jpeg_dimensions(jpeg(1280, 1829)[:-12])

    def test_an_archive_of_pages_reads_back_in_order(self) -> None:
        root = Path(tempfile.mkdtemp(prefix="smoke-pages-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        path = root / "out.zip"
        with zipfile.ZipFile(path, "w") as archive:
            archive.writestr("b.jpg", jpeg(1280, 1829))
            archive.writestr("a.jpg", jpeg(1280, 1000))
        self.assertEqual([page.name for page in read_pages(path)], ["b.jpg", "a.jpg"])


@POSIX_ONLY
class BinarySmokeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="smoke-binary-"))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)

    def stand_in(self, **overrides: object) -> Path:
        settings: dict[str, object] = {
            "version": VERSION,
            "pages": FIXTURE_PAGES,
            "width": DEFAULT_AUTO_WIDTH,
            "height": round(FIXTURE_HEIGHT * DEFAULT_AUTO_WIDTH / FIXTURE_WIDTH),
            "mode": "ok",
            "split_mode": "ok",
            "spread_width": SPREAD_WIDTH,
            "target": DEFAULT_AUTO_WIDTH,
        }
        settings.update(overrides)
        path = self.root / f"stand-in-{len(list(self.root.iterdir()))}"
        path.write_text(STAND_IN.format(**settings), encoding="utf-8")
        path.chmod(0o755)
        return path

    def work(self, name: str) -> Path:
        return self.root / f"work-{name}"

    def test_a_correct_binary_passes(self) -> None:
        pages = smoke_binary(
            self.stand_in(), version=VERSION, work_directory=self.work("ok")
        )
        self.assertEqual(len(pages), FIXTURE_PAGES)
        self.assertTrue(all(page.width == DEFAULT_AUTO_WIDTH for page in pages))

    def test_a_binary_reporting_another_version_fails(self) -> None:
        """This is the check that catches packaging a stale build.

        The `-dev` spelling is the one that actually happens: it is what the development
        line carries until a release is prepared.
        """
        for version in ("2.0.1", "2.0.0-dev"):
            with self.subTest(version=version), self.assertRaises(SmokeError):
                smoke_binary(
                    self.stand_in(version=version),
                    version=VERSION,
                    work_directory=self.work(f"version-{version}"),
                )

    def test_a_binary_whose_help_lost_an_option_fails(self) -> None:
        binary = self.stand_in()
        binary.write_text(
            binary.read_text(encoding="utf-8").replace("--auto-width ", ""),
            encoding="utf-8",
        )
        binary.chmod(0o755)
        with self.assertRaises(SmokeError):
            smoke_binary(binary, version=VERSION, work_directory=self.work("help"))

    def test_a_conversion_that_did_not_resize_fails(self) -> None:
        """Full-width output is what a broken resize path produces, and it is not an error."""
        with self.assertRaises(SmokeError):
            smoke_binary(
                self.stand_in(width=FIXTURE_WIDTH, height=FIXTURE_HEIGHT),
                version=VERSION,
                work_directory=self.work("unresized"),
            )

    def test_a_conversion_that_dropped_a_page_fails(self) -> None:
        with self.assertRaises(SmokeError):
            smoke_binary(
                self.stand_in(pages=FIXTURE_PAGES - 1),
                version=VERSION,
                work_directory=self.work("dropped"),
            )

    def test_a_conversion_that_wrote_nothing_fails(self) -> None:
        with self.assertRaises(SmokeError):
            smoke_binary(
                self.stand_in(mode="no-output"),
                version=VERSION,
                work_directory=self.work("no-output"),
            )

    def test_a_conversion_that_consumed_its_input_fails(self) -> None:
        """The tool's promise is a separate archive; a smoke test has to hold it to that."""
        with self.assertRaises(SmokeError):
            smoke_binary(
                self.stand_in(mode="eat-input"),
                version=VERSION,
                work_directory=self.work("eat-input"),
            )

    def test_a_missing_executable_fails(self) -> None:
        with self.assertRaises(release_assets.ReleaseError):
            smoke_binary(
                self.root / "absent", version=VERSION, work_directory=self.work("absent")
            )

    def test_a_binary_that_leaves_the_spread_whole_fails(self) -> None:
        """Ignoring --split is what a binary predating the feature does, and it exits 0.

        Routed through `smoke_binary` rather than `smoke_spread`, so dropping the split
        checks out of the packaged and downloaded path cannot pass this suite.
        """
        with self.assertRaises(SmokeError):
            smoke_binary(
                self.stand_in(split_mode="whole"),
                version=VERSION,
                work_directory=self.work("spread-whole"),
            )

    def test_a_binary_with_wrong_piece_geometry_fails(self) -> None:
        with self.assertRaises(SmokeError):
            smoke_spread(
                self.stand_in(split_mode="geometry"),
                work_directory=self.work("spread-geometry"),
            )

    def test_a_binary_that_ignores_the_reading_order_fails(self) -> None:
        """An accepted-but-inert flag writes the two pieces in the same order every time."""
        with self.assertRaises(SmokeError):
            smoke_spread(
                self.stand_in(split_mode="inert-order"),
                work_directory=self.work("spread-order"),
            )

    def test_a_binary_that_does_not_shift_both_windows_fails(self) -> None:
        """--split-pos does not change piece dimensions, so only the pixels can catch it.

        `one-side-offset` is the shape a whole-list comparison misses: one piece moves, so
        the two runs differ, yet a window the option promised to shift did not.
        """
        for mode in ("inert-offset", "one-side-offset"):
            with self.subTest(mode=mode), self.assertRaises(SmokeError):
                smoke_spread(
                    self.stand_in(split_mode=mode),
                    work_directory=self.work(f"spread-{mode}"),
                )


class ExtractionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="smoke-extract-"))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        self.repository = self.root / "repository"
        self.repository.mkdir()
        (self.repository / release_assets.LICENSE_FILE).write_text("licence\n", encoding="utf-8")
        (self.repository / release_assets.NOTICE_FILE).write_text("notice\n", encoding="utf-8")
        self.third_party = self.root / release_assets.THIRD_PARTY_FILE
        self.third_party.write_text("third-party\n", encoding="utf-8")

    def build(self, target) -> Path:
        binaries = self.root / f"bin-{target.name}"
        binaries.mkdir()
        path = binaries / release_assets.executable_name(target)
        path.write_bytes(
            portable_executable("KERNEL32.dll")
            if target.extension == ".zip"
            else macho("/usr/lib/libSystem.B.dylib")
        )
        path.chmod(0o755)
        return release_assets.package_release(
            self.repository,
            binaries,
            self.third_party,
            self.root / f"out-{target.name}",
            target=target,
            version=VERSION,
            tag=TAG,
            commit=COMMIT,
        )

    @POSIX_ONLY
    def test_extraction_restores_the_executable_bit(self) -> None:
        """A ZIP round trip drops the mode, and macOS refuses to run what it produced."""
        for target in release_assets.TARGETS:
            with self.subTest(target=target.triple):
                archive = self.build(target)
                extracted = extract_package(
                    archive, target, TAG, self.root / f"extracted-{target.name}"
                )
                self.assertTrue(extracted.stat().st_mode & 0o111)
                for name in release_assets.expected_file_names(target):
                    self.assertTrue((extracted.parent / name).is_file())


if __name__ == "__main__":
    unittest.main(verbosity=2)
