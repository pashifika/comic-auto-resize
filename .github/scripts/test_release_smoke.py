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
    SmokeError,
    extract_package,
    jpeg_dimensions,
    png_page,
    read_pages,
    smoke_binary,
    write_fixture,
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
HELP = "--auto-width --ratio --out --quality"
PAGES = {pages}
WIDTH = {width}
HEIGHT = {height}
MODE = {mode!r}


def jpeg(width, height):
    application = b"JFIF\\0\\x01\\x02\\x00\\x00\\x01\\x00\\x01\\x00\\x00"
    frame = struct.pack(">BHHBB", 8, height, width, 1, 0)
    return (
        b"\\xff\\xd8\\xff\\xe0"
        + struct.pack(">H", len(application) + 2)
        + application
        + b"\\xff\\xc2"
        + struct.pack(">H", len(frame) + 2)
        + frame
        + b"\\xff\\xd9"
    )


argument = sys.argv[1]
if argument == "--version":
    print(f"comic-auto-resize {{VERSION}}")
    raise SystemExit(0)
if argument == "--help":
    print(HELP)
    raise SystemExit(0)

if MODE == "no-output":
    print(f"{{PAGES}} page(s) written to nowhere")
    raise SystemExit(0)
if MODE == "eat-input":
    open(argument, "wb").close()

output = argument[: -len(".zip")] + "_resize.zip"
with zipfile.ZipFile(output, "w") as archive:
    for index in range(PAGES):
        archive.writestr(f"page-{{index + 1:03d}}.jpg", jpeg(WIDTH, HEIGHT))
print(f"{{PAGES}} page(s) written to {{output}}")
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
