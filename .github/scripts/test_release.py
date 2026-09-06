#!/usr/bin/env python3
"""Self-tests for the release provenance and packaging boundary.

These run offline, in a job with no runner secrets, no Rust toolchain, and no network.
What they defend is the set of mistakes that would otherwise be discovered by a published
release: the wrong commit, an incomplete package, a binary that needs a redistributable,
or a set whose checksums nobody rechecked.
"""

from __future__ import annotations

import re
import shutil
import stat
import struct
import tempfile
import unittest
import zipfile
from pathlib import Path

from release import (
    CHECKSUM_FILE,
    LICENSE_FILE,
    NOTICE_FILE,
    PRODUCT_NAME,
    TARGETS,
    THIRD_PARTY_FILE,
    VERSION_FILE,
    ReleaseError,
    aggregate_release,
    asset_name,
    asset_stem,
    evaluate_preflight,
    executable_name,
    expected_archive_names,
    expected_file_names,
    inspect_archive,
    inspect_linkage,
    license_texts,
    macos_loaded_dylibs,
    package_release,
    target_for,
    verify_release_set,
    windows_imported_libraries,
    workflow_outputs,
)

COMMIT = "a" * 40
OTHER_COMMIT = "b" * 40
VERSION = "2.0.0"
TAG = "v2.0.0"
WINDOWS = target_for("x86_64-pc-windows-msvc")
MACOS = target_for("aarch64-apple-darwin")
REPOSITORY_ROOT = Path(__file__).resolve().parents[2]


def valid_push(**overrides: object) -> dict[str, object]:
    """Return preflight inputs for a release that must be allowed to publish."""

    arguments: dict[str, object] = {
        "event_name": "push",
        "ref_name": TAG,
        "commit": COMMIT,
        "package_version": VERSION,
        "tag_commit": COMMIT,
        "main_contains_commit": True,
        "workflow_files_match_main": True,
        "default_branch": "main",
        "main_ci_succeeded": True,
    }
    arguments.update(overrides)
    return arguments


def macho(*dylibs: str) -> bytes:
    """Return a thin arm64 Mach-O image that loads exactly *dylibs*."""

    commands = b""
    for path in dylibs:
        raw = path.encode() + b"\0"
        padding = (-len(raw)) % 8
        size = 24 + len(raw) + padding
        commands += struct.pack("<IIIIII", 0x0C, size, 24, 0, 0, 0)
        commands += raw + b"\0" * padding
    header = struct.pack(
        "<IiiIIIII", 0xFEEDFACF, 0x0100000C, 0, 2, len(dylibs), len(commands), 0, 0
    )
    return header + commands


def portable_executable(*imports: str) -> bytes:
    """Return a PE32+ image whose import directory names exactly *imports*."""

    section_rva = 0x1000
    section_offset = 0x400
    descriptors = b""
    names = b""
    name_base = 20 * (len(imports) + 1)
    for name in imports:
        descriptors += struct.pack(
            "<IIIII", 0, 0, 0, section_rva + name_base + len(names), 0
        )
        names += name.encode() + b"\0"
    descriptors += b"\0" * 20
    section = descriptors + names
    section += b"\0" * ((-len(section)) % 0x200)

    # PE32+ optional header, by offset: magic and linker versions, five size/address
    # words, ImageBase, alignment through DllCharacteristics, stack and heap words,
    # LoaderFlags and NumberOfRvaAndSizes, then the sixteen data directories.
    optional = struct.pack("<HBB", 0x20B, 14, 0) + b"\0" * 20
    optional += struct.pack("<Q", 0x140000000) + b"\0" * 40
    optional += b"\0" * 32
    optional += struct.pack("<II", 0, 16)
    directories = bytearray(16 * 8)
    struct.pack_into("<II", directories, 8, section_rva, len(descriptors))
    optional += bytes(directories)

    section_header = b".rdata\0\0" + struct.pack(
        "<IIII", len(section), section_rva, len(section), section_offset
    )
    section_header += b"\0" * 16

    coff = struct.pack("<HHIIIHH", 0x8664, 1, 0, 0, 0, len(optional), 0x22)
    headers = b"MZ" + b"\0" * 0x3A + struct.pack("<I", 0x40)
    headers += b"PE\0\0" + coff + optional + section_header
    headers += b"\0" * (section_offset - len(headers))
    return headers + section


class PreflightPolicyTests(unittest.TestCase):
    def test_a_valid_stable_tag_publishes(self) -> None:
        result = evaluate_preflight(**valid_push())
        self.assertTrue(result.publish)
        self.assertEqual((result.tag, result.version, result.commit), (TAG, VERSION, COMMIT))

    def test_workflow_outputs_carry_both_targets_and_no_newline(self) -> None:
        """A dropped matrix row would publish a one-platform release as if complete."""
        outputs = workflow_outputs(evaluate_preflight(**valid_push()))
        self.assertEqual(outputs["publish"], "true")
        for triple in (target.triple for target in TARGETS):
            self.assertIn(triple, outputs["matrix"])
        for value in outputs.values():
            self.assertNotIn("\n", value)

    def test_a_tag_that_is_not_a_stable_version_is_refused(self) -> None:
        for ref in ("v2.0", "v2.0.0-rc.1", "2.0.0", "v02.0.0", "v2.0.0+build", "latest"):
            with self.subTest(ref=ref), self.assertRaises(ReleaseError):
                evaluate_preflight(**valid_push(ref_name=ref))

    def test_a_tag_that_disagrees_with_cargo_is_refused(self) -> None:
        with self.assertRaises(ReleaseError):
            evaluate_preflight(**valid_push(package_version="2.0.1"))

    def test_a_tag_pointing_elsewhere_is_refused(self) -> None:
        """The event, the tag, and the checkout must be one commit, not two."""
        with self.assertRaises(ReleaseError):
            evaluate_preflight(**valid_push(tag_commit=OTHER_COMMIT))

    def test_an_off_main_tag_is_refused(self) -> None:
        with self.assertRaises(ReleaseError):
            evaluate_preflight(**valid_push(main_contains_commit=False))

    def test_publication_requires_main_to_be_the_default_branch(self) -> None:
        with self.assertRaises(ReleaseError):
            evaluate_preflight(**valid_push(default_branch="master"))

    def test_publication_requires_ci_for_the_exact_commit(self) -> None:
        """A green pull request is evidence about a different commit."""
        with self.assertRaises(ReleaseError):
            evaluate_preflight(**valid_push(main_ci_succeeded=False))

    def test_a_divergent_workflow_tree_is_refused(self) -> None:
        with self.assertRaises(ReleaseError):
            evaluate_preflight(**valid_push(workflow_files_match_main=False))

    def test_unknown_provenance_is_not_treated_as_satisfied(self) -> None:
        """`None` means "never checked"; only `True` may publish."""
        for field in (
            "main_contains_commit",
            "workflow_files_match_main",
            "main_ci_succeeded",
        ):
            with self.subTest(field=field), self.assertRaises(ReleaseError):
                evaluate_preflight(**valid_push(**{field: None}))

    def test_manual_dispatch_never_publishes(self) -> None:
        """A rehearsal that selected a real release tag still must not publish."""
        result = evaluate_preflight(
            event_name="workflow_dispatch",
            ref_name=TAG,
            commit=COMMIT,
            package_version=VERSION,
            tag_commit=None,
            main_contains_commit=None,
            workflow_files_match_main=None,
            default_branch=None,
            main_ci_succeeded=None,
        )
        self.assertFalse(result.publish)
        self.assertEqual(workflow_outputs(result)["publish"], "false")

    def test_manual_dispatch_requires_a_ref(self) -> None:
        for ref in ("", "  ", "main branch"):
            with self.subTest(ref=ref), self.assertRaises(ReleaseError):
                evaluate_preflight(
                    event_name="workflow_dispatch",
                    ref_name=ref,
                    commit=COMMIT,
                    package_version=VERSION,
                    tag_commit=None,
                    main_contains_commit=None,
                    workflow_files_match_main=None,
                    default_branch=None,
                    main_ci_succeeded=None,
                )

    def test_an_unsupported_event_is_refused(self) -> None:
        for event in ("release", "schedule", "pull_request"):
            with self.subTest(event=event), self.assertRaises(ReleaseError):
                evaluate_preflight(**valid_push(event_name=event))


class LinkageTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = Path(tempfile.mkdtemp(prefix="release-linkage-"))
        self.addCleanup(shutil.rmtree, self.directory, ignore_errors=True)

    def write(self, name: str, data: bytes) -> Path:
        path = self.directory / name
        path.write_bytes(data)
        path.chmod(0o755)
        return path

    def test_a_system_only_macho_is_accepted(self) -> None:
        path = self.write("mac", macho("/usr/lib/libSystem.B.dylib", "/usr/lib/libc++.1.dylib"))
        self.assertEqual(
            macos_loaded_dylibs(path),
            ("/usr/lib/libSystem.B.dylib", "/usr/lib/libc++.1.dylib"),
        )
        self.assertEqual(len(inspect_linkage(path, MACOS)), 2)

    def test_a_build_machine_dylib_is_refused(self) -> None:
        """A Homebrew path resolves on the runner and on nobody else's Mac."""
        for path in ("/opt/homebrew/lib/libpng16.dylib", "/usr/local/lib/x.dylib", "@rpath/y.dylib"):
            binary = self.write("mac", macho("/usr/lib/libSystem.B.dylib", path))
            with self.subTest(path=path), self.assertRaises(ReleaseError):
                inspect_linkage(binary, MACOS)

    def test_a_static_crt_pe_is_accepted(self) -> None:
        path = self.write("win.exe", portable_executable("KERNEL32.dll", "bcrypt.dll"))
        self.assertEqual(windows_imported_libraries(path), ("KERNEL32.dll", "bcrypt.dll"))
        self.assertEqual(len(inspect_linkage(path, WINDOWS)), 2)

    def test_a_dynamic_crt_pe_is_refused(self) -> None:
        """These imports are exactly what a dropped `+crt-static` flag looks like."""
        for name in (
            "VCRUNTIME140.dll",
            "MSVCP140.dll",
            "api-ms-win-crt-runtime-l1-1-0.dll",
            "ucrtbase.dll",
        ):
            binary = self.write("win.exe", portable_executable("KERNEL32.dll", name))
            with self.subTest(name=name), self.assertRaises(ReleaseError):
                inspect_linkage(binary, WINDOWS)

    def test_an_image_that_imports_nothing_is_refused(self) -> None:
        """A parser bug that returned an empty list would otherwise pass every check."""
        binary = self.write("win.exe", portable_executable())
        with self.assertRaises(ReleaseError):
            inspect_linkage(binary, WINDOWS)
        with self.assertRaises(ReleaseError):
            inspect_linkage(self.write("mac", macho()), MACOS)

    def test_a_non_image_is_refused(self) -> None:
        binary = self.write("win.exe", b"#!/bin/sh\necho not a binary\n")
        with self.assertRaises(ReleaseError):
            inspect_linkage(binary, WINDOWS)
        with self.assertRaises(ReleaseError):
            inspect_linkage(binary, MACOS)


class PackagingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="release-package-"))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        self.repository = self.root / "repository"
        self.repository.mkdir()
        (self.repository / LICENSE_FILE).write_text("Apache-2.0 text\n", encoding="utf-8")
        (self.repository / NOTICE_FILE).write_text("UnRAR notice\n", encoding="utf-8")
        self.third_party = self.root / THIRD_PARTY_FILE
        self.third_party.write_text("third-party licences\n", encoding="utf-8")
        self.binaries = self.root / "bin"
        self.binaries.mkdir()

    def build_binary(self, target) -> Path:
        path = self.binaries / executable_name(target)
        if target.extension == ".zip":
            path.write_bytes(portable_executable("KERNEL32.dll"))
        else:
            path.write_bytes(macho("/usr/lib/libSystem.B.dylib"))
        path.chmod(0o755)
        return path

    def package(self, target, output: Path, *, commit: str = COMMIT) -> Path:
        self.build_binary(target)
        return package_release(
            self.repository,
            self.binaries,
            self.third_party,
            output,
            target=target,
            version=VERSION,
            tag=TAG,
            commit=commit,
        )

    def test_a_package_holds_exactly_the_promised_members(self) -> None:
        for target in TARGETS:
            with self.subTest(target=target.triple):
                archive = self.package(target, self.root / f"out-{target.name}")
                self.assertEqual(archive.name, asset_name(TAG, target))
                inspect_archive(
                    archive, target=target, version=VERSION, tag=TAG, commit=COMMIT
                )
                self.assertEqual(
                    expected_file_names(target),
                    (
                        LICENSE_FILE,
                        NOTICE_FILE,
                        THIRD_PARTY_FILE,
                        VERSION_FILE,
                        executable_name(target),
                    ),
                )

    def test_identical_inputs_repackage_to_identical_bytes(self) -> None:
        first = self.package(WINDOWS, self.root / "first")
        second = self.package(WINDOWS, self.root / "second")
        self.assertEqual(first.read_bytes(), second.read_bytes())

    def test_a_package_built_for_another_commit_is_refused(self) -> None:
        """The archive is the only place the source commit is bound to the bytes."""
        archive = self.package(MACOS, self.root / "out")
        with self.assertRaises(ReleaseError):
            inspect_archive(
                archive, target=MACOS, version=VERSION, tag=TAG, commit=OTHER_COMMIT
            )

    def test_a_package_labelled_with_another_version_or_target_is_refused(self) -> None:
        archive = self.package(MACOS, self.root / "out")
        with self.assertRaises(ReleaseError):
            inspect_archive(
                archive, target=MACOS, version="2.0.1", tag="v2.0.1", commit=COMMIT
            )
        renamed = archive.parent / asset_name(TAG, WINDOWS)
        shutil.copyfile(archive, renamed)
        with self.assertRaises(ReleaseError):
            inspect_archive(
                renamed, target=WINDOWS, version=VERSION, tag=TAG, commit=COMMIT
            )

    def test_a_missing_notice_stops_packaging(self) -> None:
        for name in (LICENSE_FILE, NOTICE_FILE):
            with self.subTest(name=name):
                (self.repository / name).rename(self.root / f"moved-{name}")
                with self.assertRaises(ReleaseError):
                    self.package(MACOS, self.root / f"out-{name}")
                (self.root / f"moved-{name}").rename(self.repository / name)

    def test_an_empty_notice_stops_packaging(self) -> None:
        """A generator that produced nothing must not ship as a notice file."""
        self.third_party.write_text("", encoding="utf-8")
        with self.assertRaises(ReleaseError):
            self.package(MACOS, self.root / "out")

    def test_a_binary_needing_a_redistributable_stops_packaging(self) -> None:
        path = self.binaries / executable_name(WINDOWS)
        path.write_bytes(portable_executable("KERNEL32.dll", "VCRUNTIME140.dll"))
        with self.assertRaises(ReleaseError):
            package_release(
                self.repository,
                self.binaries,
                self.third_party,
                self.root / "out",
                target=WINDOWS,
                version=VERSION,
                tag=TAG,
                commit=COMMIT,
            )

    def test_a_non_executable_macos_binary_stops_packaging(self) -> None:
        path = self.build_binary(MACOS)
        path.chmod(0o644)
        with self.assertRaises(ReleaseError):
            package_release(
                self.repository,
                self.binaries,
                self.third_party,
                self.root / "out",
                target=MACOS,
                version=VERSION,
                tag=TAG,
                commit=COMMIT,
            )

    def test_an_altered_archive_is_refused(self) -> None:
        """Repacking by hand is how an extra file or a lost mode gets in."""
        archive = self.package(WINDOWS, self.root / "out")
        root = asset_stem(TAG, WINDOWS)
        for case in ("extra", "missing", "mode"):
            tampered = self.root / f"{case}-{archive.name}"
            with zipfile.ZipFile(archive) as source, zipfile.ZipFile(
                tampered, "w"
            ) as sink:
                for info in source.infolist():
                    if case == "missing" and info.filename.endswith(NOTICE_FILE):
                        continue
                    copied = zipfile.ZipInfo(info.filename, info.date_time)
                    copied.create_system = info.create_system
                    copied.external_attr = info.external_attr
                    if case == "mode" and info.filename.endswith(
                        executable_name(WINDOWS)
                    ):
                        copied.external_attr = (
                            (stat.S_IFREG | 0o644) << 16
                        )
                    sink.writestr(copied, source.read(info))
                if case == "extra":
                    sink.writestr(f"{root}/README.txt", b"unexpected\n")
            with self.subTest(case=case), self.assertRaises(ReleaseError):
                inspect_archive(
                    tampered, target=WINDOWS, version=VERSION, tag=TAG, commit=COMMIT
                )


class AggregationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="release-aggregate-"))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        self.repository = self.root / "repository"
        self.repository.mkdir()
        (self.repository / LICENSE_FILE).write_text("Apache-2.0 text\n", encoding="utf-8")
        (self.repository / NOTICE_FILE).write_text("UnRAR notice\n", encoding="utf-8")
        self.third_party = self.root / THIRD_PARTY_FILE
        self.third_party.write_text("third-party licences\n", encoding="utf-8")
        self.input = self.root / "input"
        self.input.mkdir()
        for target in TARGETS:
            binaries = self.root / f"bin-{target.name}"
            binaries.mkdir()
            path = binaries / executable_name(target)
            path.write_bytes(
                portable_executable("KERNEL32.dll")
                if target.extension == ".zip"
                else macho("/usr/lib/libSystem.B.dylib")
            )
            path.chmod(0o755)
            package_release(
                self.repository,
                binaries,
                self.third_party,
                self.input,
                target=target,
                version=VERSION,
                tag=TAG,
                commit=COMMIT,
            )

    def test_a_complete_set_aggregates_and_verifies(self) -> None:
        output = self.root / "verified"
        archives = aggregate_release(
            self.input, output, version=VERSION, tag=TAG, commit=COMMIT
        )
        self.assertEqual(len(archives), len(TARGETS))
        self.assertEqual(
            sorted(path.name for path in output.iterdir()),
            sorted([*expected_archive_names(TAG), CHECKSUM_FILE]),
        )
        checksums = (output / CHECKSUM_FILE).read_text(encoding="ascii")
        self.assertEqual(len(checksums.splitlines()), len(TARGETS))
        for line in checksums.splitlines():
            self.assertRegex(line, r"^[0-9a-f]{64}  [^/\\]+$")
        verify_release_set(output, version=VERSION, tag=TAG, commit=COMMIT)

    def test_one_target_alone_is_not_a_release(self) -> None:
        (self.input / asset_name(TAG, MACOS)).unlink()
        with self.assertRaises(ReleaseError):
            aggregate_release(
                self.input, self.root / "verified", version=VERSION, tag=TAG, commit=COMMIT
            )

    def test_an_unexpected_artifact_stops_aggregation(self) -> None:
        (self.input / "comic-auto-resize-v2.0.0-extra.zip").write_bytes(b"stray\n")
        with self.assertRaises(ReleaseError):
            aggregate_release(
                self.input, self.root / "verified", version=VERSION, tag=TAG, commit=COMMIT
            )

    def test_a_corrupt_checksum_fails_verification(self) -> None:
        """The published `SHA256SUMS` is what a user checks; it must not drift."""
        output = self.root / "verified"
        aggregate_release(self.input, output, version=VERSION, tag=TAG, commit=COMMIT)
        checksum_path = output / CHECKSUM_FILE
        text = checksum_path.read_text(encoding="ascii")
        checksum_path.write_text("0" * 64 + text[64:], encoding="ascii")
        with self.assertRaises(ReleaseError):
            verify_release_set(output, version=VERSION, tag=TAG, commit=COMMIT)

    def test_a_missing_or_extra_asset_fails_verification(self) -> None:
        output = self.root / "verified"
        aggregate_release(self.input, output, version=VERSION, tag=TAG, commit=COMMIT)
        (output / "notes.txt").write_text("extra\n", encoding="utf-8")
        with self.assertRaises(ReleaseError):
            verify_release_set(output, version=VERSION, tag=TAG, commit=COMMIT)
        (output / "notes.txt").unlink()
        (output / CHECKSUM_FILE).unlink()
        with self.assertRaises(ReleaseError):
            verify_release_set(output, version=VERSION, tag=TAG, commit=COMMIT)


class LicenseBundleTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="release-licenses-"))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)

    def package_at(self, name: str) -> dict[str, object]:
        crate = self.root / name
        crate.mkdir()
        (crate / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
        return {"name": name, "manifest_path": str(crate / "Cargo.toml")}

    def test_a_crate_with_no_licence_text_fails_the_bundle(self) -> None:
        """`cargo deny` passes such a crate on metadata alone; a package cannot."""
        with self.assertRaises(ReleaseError):
            license_texts(self.package_at("silent-crate"))

    def test_root_licence_text_is_collected(self) -> None:
        package = self.package_at("ordinary-crate")
        root = Path(str(package["manifest_path"])).parent
        (root / "LICENSE-MIT").write_text("MIT text\n", encoding="utf-8")
        (root / "Cargo.toml.orig").write_text("[package]\n", encoding="utf-8")
        self.assertEqual(
            license_texts(package), [("LICENSE-MIT", "MIT text\n")]
        )

    def test_a_recorded_vendored_path_is_required_to_exist(self) -> None:
        """The UnRAR obligation travels with the binary or the release stops."""
        package = self.package_at("unrar-ng-sys")
        with self.assertRaises(ReleaseError):
            license_texts(package)
        vendored = Path(str(package["manifest_path"])).parent / "vendor" / "unrar"
        vendored.mkdir(parents=True)
        (vendored / "license.txt").write_text("UnRAR clause 2\n", encoding="utf-8")
        self.assertEqual(
            license_texts(package), [("vendor/unrar/license.txt", "UnRAR clause 2\n")]
        )


class WorkflowInvariantTests(unittest.TestCase):
    """Facts the two workflow files must agree on, checked rather than remembered."""

    def setUp(self) -> None:
        self.ci = (REPOSITORY_ROOT / ".github/workflows/ci.yml").read_text(
            encoding="utf-8"
        )
        self.release = (REPOSITORY_ROOT / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )

    def pins(self, text: str) -> dict[str, str]:
        return {
            name: value
            for name, value in re.findall(
                r"^\s+(NASM_PACKAGE_VERSION|NASM_VERSION|NASM_NUPKG_SHA512):\s*\"?([^\"\n]+)\"?$",
                text,
                re.MULTILINE,
            )
        }

    def test_both_workflows_provision_the_same_nasm(self) -> None:
        """Bumping one copy alone would ship a differently assembled mozjpeg."""
        ci_pins = self.pins(self.ci)
        release_pins = self.pins(self.release)
        self.assertEqual(len(ci_pins), 3, "the CI nasm pin no longer parses")
        self.assertEqual(ci_pins, release_pins)

    def test_the_ci_gate_requires_the_offline_release_checks(self) -> None:
        """`ci` is the only required context, so a new job counts only through it."""
        gate = re.search(r"\n  ci:\n(.*?)\n    steps:\n", self.ci, re.DOTALL)
        self.assertIsNotNone(gate, "the ci gate job no longer parses")
        assert gate is not None
        needs = re.search(r"needs:\s*\[([^\]]+)\]", gate.group(1), re.DOTALL)
        self.assertIsNotNone(needs, "the ci gate declares no needs list")
        assert needs is not None
        required = {name.strip() for name in needs.group(1).split(",") if name.strip()}
        self.assertIn("release-validation", required)
        # The gate reads each result by name, so a job added to `needs` and forgotten in
        # the body would still let a failure through.
        self.assertIn("release-validation=${RELEASE_VALIDATION_RESULT}", self.ci)

    def test_the_release_workflow_defaults_to_read_only_permissions(self) -> None:
        self.assertRegex(self.release, r"\npermissions:\n  contents: read\n")
        self.assertEqual(self.release.count("contents: write"), 1)

    def test_the_product_name_matches_the_manifest(self) -> None:
        manifest = (REPOSITORY_ROOT / "Cargo.toml").read_text(encoding="utf-8")
        self.assertIn(f'name = "{PRODUCT_NAME}"', manifest)


if __name__ == "__main__":
    unittest.main(verbosity=2)
