#!/usr/bin/env python3
"""Validate provenance, package, and aggregate comic-auto-resize release artifacts.

The release boundary is asymmetric: everything here may refuse a publication, and nothing
here may create, move, or delete a tag. Provenance is therefore checked twice — once before
any native work, once again immediately before the publisher touches a GitHub Release.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
import re
import shutil
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import zipfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Sequence

PRODUCT_NAME = "comic-auto-resize"
LICENSE_FILE = "LICENSE"
NOTICE_FILE = "NOTICE.md"
THIRD_PARTY_FILE = "THIRD-PARTY-LICENSES.txt"
VERSION_FILE = "VERSION"
CHECKSUM_FILE = "SHA256SUMS"
CI_WORKFLOW_PATH = ".github/workflows/ci.yml"
WORKFLOW_DIRECTORY = ".github/workflows"
ZIP_TIMESTAMP = (1980, 1, 1, 0, 0, 0)
API_VERSION = "2026-03-10"

# Anchored, and deliberately narrower than the `v*` push filter: GitHub tag triggers are
# globs, so the grammar that decides what may publish lives here rather than in YAML.
TAG_PATTERN = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
COMMIT_PATTERN = re.compile(r"[0-9a-f]{40}")

# Licence-text file names as they appear at a crate's own root. `cargo deny` reads only
# `LICENSE*`/`COPYING*` there; this is the same idea with the two other spellings crates
# actually use.
LICENSE_TEXT_PREFIXES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE")
# `.orig` files are Cargo's pre-normalization manifest copies, never licence text.
LICENSE_TEXT_EXCLUDED_SUFFIXES = (".orig",)


class ReleaseError(RuntimeError):
    """A release invariant was not satisfied."""


@dataclass(frozen=True)
class Target:
    """One supported native release target."""

    name: str
    runner: str
    runner_arch: str
    host: str
    triple: str
    extension: str
    executable_suffix: str

    def matrix_entry(self) -> dict[str, str]:
        """Return the GitHub Actions matrix row for this target."""

        return {
            "name": self.name,
            "runner": self.runner,
            "runner_arch": self.runner_arch,
            "host": self.host,
            "target": self.triple,
        }


# The two targets `CLAUDE.md` names, on the two runner labels `ci.yml` already verifies
# natively. Neither is cross-compiled, so runner and host are asserted rather than assumed.
TARGETS = (
    Target(
        name="windows-x64",
        runner="windows-2025",
        runner_arch="X64",
        host="x86_64-pc-windows-msvc",
        triple="x86_64-pc-windows-msvc",
        extension=".zip",
        executable_suffix=".exe",
    ),
    Target(
        name="macos-arm64",
        runner="macos-15",
        runner_arch="ARM64",
        host="aarch64-apple-darwin",
        triple="aarch64-apple-darwin",
        extension=".tar.gz",
        executable_suffix="",
    ),
)
TARGET_BY_TRIPLE = {target.triple: target for target in TARGETS}


@dataclass(frozen=True)
class VendoredLicense:
    """Licence text a crate carries somewhere other than its own root."""

    paths: tuple[str, ...]
    reason: str


# `cargo deny` reads a crate's root and does not recurse, so a vendored tree passes on the
# strength of its own manifest. `NOTICE.md` records that gap for the reader; this table
# records it for the package, and a missing path here fails packaging rather than shipping
# a binary whose obligations did not travel with it.
VENDORED_LICENSES = {
    "unrar-ng-sys": VendoredLicense(
        paths=("vendor/unrar/license.txt",),
        reason=(
            "ships no licence text at its own crate root; builds RARLAB's modified UnRAR "
            "from the vendored tree below. Its own MIT OR Apache-2.0 text is carried by "
            "unrar-ng, the root crate of the same repository, listed in this file."
        ),
    ),
}

# Windows runtime libraries that must not appear in the shipped import table. The release
# build is `-C target-feature=+crt-static`, so every one of these is evidence that the
# static CRT did not take effect and the binary needs a redistributable the download
# instructions do not mention.
WINDOWS_FORBIDDEN_IMPORT_PREFIXES = (
    "api-ms-win-crt-",
    "concrt",
    "msvcp",
    "msvcr",
    "ucrtbase",
    "vcruntime",
)
# macOS system library roots. Anything else — Homebrew, `/usr/local`, an `@rpath` entry —
# is a build-machine path that will not exist on a user's Mac.
MACOS_ALLOWED_DYLIB_ROOTS = ("/usr/lib/", "/System/Library/")


@dataclass(frozen=True)
class PreflightResult:
    """Validated values safe to expose as workflow outputs."""

    version: str
    tag: str
    commit: str
    publish: bool


def stable_version_from_tag(tag: str) -> str:
    """Return a stable semantic version from an exact release tag."""

    match = TAG_PATTERN.fullmatch(tag)
    if match is None:
        raise ReleaseError(
            f"release tag {tag!r} must match vMAJOR.MINOR.PATCH without "
            "prerelease metadata or leading zeroes"
        )
    return ".".join(match.groups())


def validate_stable_version(version: str) -> str:
    """Validate an unprefixed stable semantic version."""

    stable_version_from_tag(f"v{version}")
    return version


def validate_commit(commit: str) -> str:
    """Validate a full lowercase Git commit object ID."""

    if COMMIT_PATTERN.fullmatch(commit) is None:
        raise ReleaseError(f"commit {commit!r} is not a full lowercase SHA-1 object ID")
    return commit


def target_for(triple: str) -> Target:
    """Return the supported target definition for *triple*."""

    try:
        return TARGET_BY_TRIPLE[triple]
    except KeyError as error:
        supported = ", ".join(target.triple for target in TARGETS)
        raise ReleaseError(
            f"unsupported release target {triple!r}; expected {supported}"
        ) from error


def asset_stem(tag: str, target: Target) -> str:
    """Return the archive's top-level directory and filename stem."""

    stable_version_from_tag(tag)
    return f"{PRODUCT_NAME}-{tag}-{target.triple}"


def asset_name(tag: str, target: Target) -> str:
    """Return the deterministic archive name for one target."""

    return f"{asset_stem(tag, target)}{target.extension}"


def expected_archive_names(tag: str) -> tuple[str, ...]:
    """Return every archive name in deterministic order."""

    return tuple(sorted(asset_name(tag, target) for target in TARGETS))


def executable_name(target: Target) -> str:
    """Return the single product executable name for a target."""

    return f"{PRODUCT_NAME}{target.executable_suffix}"


def expected_file_names(target: Target) -> tuple[str, ...]:
    """Return the exact package file set in archive order."""

    return tuple(
        sorted(
            (
                executable_name(target),
                LICENSE_FILE,
                NOTICE_FILE,
                THIRD_PARTY_FILE,
                VERSION_FILE,
            )
        )
    )


def expected_member_names(tag: str, target: Target) -> tuple[str, ...]:
    """Return the exact normalized archive member order."""

    root = asset_stem(tag, target)
    return (f"{root}/", *(f"{root}/{name}" for name in expected_file_names(target)))


def version_metadata(version: str, tag: str, target: Target, commit: str) -> bytes:
    """Return deterministic release metadata stored in every package."""

    validate_stable_version(version)
    if stable_version_from_tag(tag) != version:
        raise ReleaseError(f"tag {tag!r} does not match package version {version!r}")
    validate_commit(commit)
    return (
        f"name={PRODUCT_NAME}\n"
        f"version={version}\n"
        f"tag={tag}\n"
        f"target={target.triple}\n"
        f"commit={commit}\n"
    ).encode()


def evaluate_preflight(
    *,
    event_name: str,
    ref_name: str,
    commit: str,
    package_version: str,
    tag_commit: str | None,
    main_contains_commit: bool | None,
    workflow_files_match_main: bool | None,
    default_branch: str | None,
    main_ci_succeeded: bool | None,
) -> PreflightResult:
    """Apply event, tag, version, and provenance policy to observed values."""

    validate_commit(commit)
    validate_stable_version(package_version)

    if event_name == "push":
        tag_version = stable_version_from_tag(ref_name)
        if tag_version != package_version:
            raise ReleaseError(
                f"tag version {tag_version!r} does not match locked Cargo package version "
                f"{package_version!r}"
            )
        if tag_commit != commit:
            raise ReleaseError(
                f"tag {ref_name!r} resolves to {tag_commit!r}, not checked-out commit {commit}"
            )
        if main_contains_commit is not True:
            raise ReleaseError(
                f"tag commit {commit} is not an ancestor of the fetched origin/main"
            )
        if default_branch != "main":
            raise ReleaseError(
                f"repository default branch is {default_branch!r}; publication requires "
                "`main`, which is also what makes this workflow reachable"
            )
        if main_ci_succeeded is not True:
            raise ReleaseError(
                f"no successful main CI run was found for the exact commit {commit}; "
                "a pull-request run for the same tree is not that evidence"
            )
        if workflow_files_match_main is not True:
            raise ReleaseError(
                f"tag commit changes {WORKFLOW_DIRECTORY} relative to origin/main; the "
                "Actions token cannot publish from a divergent workflow tree"
            )
        return PreflightResult(
            version=package_version, tag=ref_name, commit=commit, publish=True
        )

    if event_name == "workflow_dispatch":
        if not ref_name or any(character.isspace() for character in ref_name):
            raise ReleaseError("manual rehearsal requires one non-whitespace ref input")
        return PreflightResult(
            version=package_version,
            tag=f"v{package_version}",
            commit=commit,
            publish=False,
        )

    raise ReleaseError(
        f"unsupported release event {event_name!r}; expected push or workflow_dispatch"
    )


def run_checked(
    arguments: Sequence[str], *, cwd: Path, input_bytes: bytes | None = None
) -> subprocess.CompletedProcess[bytes]:
    """Run one shell-free command and retain bounded diagnostic output."""

    completed = subprocess.run(
        arguments,
        cwd=cwd,
        input=input_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode != 0:
        stderr = completed.stderr.decode(errors="replace").strip()
        command = " ".join(arguments[:3])
        raise ReleaseError(
            f"{command} failed with status {completed.returncode}: {stderr}"
        )
    return completed


def git_output(repository: Path, *arguments: str) -> str:
    """Run Git and return trimmed UTF-8 output."""

    completed = run_checked(("git", *arguments), cwd=repository)
    try:
        return completed.stdout.decode().strip()
    except UnicodeDecodeError as error:
        raise ReleaseError("git output was not valid UTF-8") from error


def locked_package_version(repository: Path) -> str:
    """Read the root package version from the manifest and the committed lockfile.

    Read directly rather than through `cargo metadata --locked`, which would install the
    pinned toolchain on a runner that otherwise needs no Rust. The stronger property —
    that the whole lockfile still resolves — is enforced by the `--locked` builds and
    tests the native jobs run against the same commit.
    """

    manifest_path = repository / "Cargo.toml"
    lock_path = repository / "Cargo.lock"
    try:
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        lockfile = tomllib.loads(lock_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        raise ReleaseError(f"cannot read Cargo manifest metadata: {error}") from error

    package = manifest.get("package")
    if not isinstance(package, dict) or package.get("name") != PRODUCT_NAME:
        raise ReleaseError(f"{manifest_path} does not declare the {PRODUCT_NAME} package")
    version = package.get("version")
    if not isinstance(version, str):
        raise ReleaseError("Cargo package version is missing or not a string")

    entries = [
        entry
        for entry in lockfile.get("package", [])
        if isinstance(entry, dict) and entry.get("name") == PRODUCT_NAME
    ]
    if len(entries) != 1:
        raise ReleaseError(
            f"expected exactly one {PRODUCT_NAME} entry in Cargo.lock, found {len(entries)}"
        )
    locked_version = entries[0].get("version")
    if locked_version != version:
        raise ReleaseError(
            f"Cargo.lock records {PRODUCT_NAME} {locked_version!r} but the manifest "
            f"declares {version!r}"
        )
    return validate_stable_version(version)


def git_is_ancestor(repository: Path, ancestor: str, descendant: str) -> bool:
    """Return whether Git proves *ancestor* is reachable from *descendant*."""

    completed = subprocess.run(
        ("git", "merge-base", "--is-ancestor", ancestor, descendant),
        cwd=repository,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode == 0:
        return True
    if completed.returncode == 1:
        return False
    stderr = completed.stderr.decode(errors="replace").strip()
    raise ReleaseError(
        f"git merge-base failed with status {completed.returncode}: {stderr}"
    )


def git_paths_match(repository: Path, left: str, right: str, *paths: str) -> bool:
    """Return whether Git proves selected paths have identical content."""

    completed = subprocess.run(
        ("git", "diff", "--quiet", left, right, "--", *paths),
        cwd=repository,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode == 0:
        return True
    if completed.returncode == 1:
        return False
    stderr = completed.stderr.decode(errors="replace").strip()
    raise ReleaseError(f"git diff failed with status {completed.returncode}: {stderr}")


class GitHubProvenance:
    """Read-only GitHub queries behind the release provenance checks."""

    def __init__(self, repository: Path) -> None:
        """Bind read-only GitHub CLI calls to a repository checkout."""

        self.repository = repository.resolve()

    def _api(self, endpoint: str, *, paginate: bool = False) -> object:
        arguments = [
            "gh",
            "api",
            endpoint,
            "--header",
            f"X-GitHub-Api-Version: {API_VERSION}",
        ]
        if paginate:
            arguments.extend(("--paginate", "--slurp"))
        output = run_checked(tuple(arguments), cwd=self.repository).stdout
        try:
            return json.loads(output)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ReleaseError(
                f"GitHub API returned invalid JSON for {endpoint}"
            ) from error

    def default_branch(self, repository: str) -> str:
        """Return the repository's current default branch."""

        value = self._api(f"repos/{repository}")
        if not isinstance(value, dict) or not isinstance(
            value.get("default_branch"), str
        ):
            raise ReleaseError("GitHub did not return a repository default branch")
        return value["default_branch"]

    def main_ci_succeeded(self, repository: str, commit: str) -> bool:
        """Return whether the CI workflow succeeded for *commit* pushed to main."""

        validate_commit(commit)
        endpoint = (
            f"repos/{repository}/actions/workflows/ci.yml/runs"
            f"?head_sha={commit}&branch=main&event=push&per_page=100"
        )
        pages = self._api(endpoint, paginate=True)
        if not isinstance(pages, list):
            raise ReleaseError("GitHub workflow-run listing was not a JSON array")
        for page in pages:
            if not isinstance(page, dict):
                raise ReleaseError("GitHub workflow-run page was not a JSON object")
            runs = page.get("workflow_runs")
            if not isinstance(runs, list):
                raise ReleaseError("GitHub workflow-run page carried no run array")
            for run in runs:
                if not isinstance(run, dict):
                    raise ReleaseError("GitHub workflow run was not a JSON object")
                if (
                    run.get("head_sha") == commit
                    and run.get("head_branch") == "main"
                    and run.get("event") == "push"
                    and run.get("path") == CI_WORKFLOW_PATH
                    and run.get("status") == "completed"
                    and run.get("conclusion") == "success"
                ):
                    return True
        return False


def verify_publication_source(
    repository: Path, commit: str, *, github_repository: str
) -> None:
    """Revalidate the checked-out commit against current remote main before publishing."""

    repository = repository.resolve()
    commit = validate_commit(commit)
    checked_out = validate_commit(git_output(repository, "rev-parse", "HEAD^{commit}"))
    if checked_out != commit:
        raise ReleaseError(
            f"checked-out commit {checked_out} does not match validated commit {commit}"
        )
    run_checked(
        (
            "git",
            "fetch",
            "--no-tags",
            "--force",
            "origin",
            "+refs/heads/main:refs/remotes/origin/main",
        ),
        cwd=repository,
    )
    if not git_is_ancestor(repository, commit, "origin/main"):
        raise ReleaseError(
            f"tag commit {commit} is not an ancestor of the fetched origin/main"
        )
    if not git_paths_match(repository, commit, "origin/main", WORKFLOW_DIRECTORY):
        raise ReleaseError(
            f"tag commit changes {WORKFLOW_DIRECTORY} relative to origin/main; the "
            "Actions token cannot publish from a divergent workflow tree"
        )
    provenance = GitHubProvenance(repository)
    default_branch = provenance.default_branch(github_repository)
    if default_branch != "main":
        raise ReleaseError(
            f"repository default branch is {default_branch!r}; publication requires `main`"
        )
    if not provenance.main_ci_succeeded(github_repository, commit):
        raise ReleaseError(
            f"no successful main CI run was found for the exact commit {commit}"
        )


def preflight(
    repository: Path,
    *,
    event_name: str,
    ref_name: str,
    event_sha: str,
    github_repository: str,
) -> PreflightResult:
    """Collect and validate release provenance from the checked-out repository."""

    repository = repository.resolve()
    commit = validate_commit(git_output(repository, "rev-parse", "HEAD^{commit}"))
    package_version = locked_package_version(repository)

    tag_commit: str | None = None
    main_contains_commit: bool | None = None
    workflow_files_match_main: bool | None = None
    default_branch: str | None = None
    main_ci_succeeded: bool | None = None

    if event_name == "push":
        stable_version_from_tag(ref_name)
        event_commit = validate_commit(
            git_output(repository, "rev-parse", f"{event_sha}^{{commit}}")
        )
        if event_commit != commit:
            raise ReleaseError(
                f"event commit {event_commit} does not match checked-out commit {commit}"
            )
        tag_commit = validate_commit(
            git_output(repository, "rev-parse", f"refs/tags/{ref_name}^{{commit}}")
        )
        run_checked(
            (
                "git",
                "fetch",
                "--no-tags",
                "--force",
                "origin",
                "+refs/heads/main:refs/remotes/origin/main",
            ),
            cwd=repository,
        )
        main_contains_commit = git_is_ancestor(repository, commit, "origin/main")
        workflow_files_match_main = git_paths_match(
            repository, commit, "origin/main", WORKFLOW_DIRECTORY
        )
        provenance = GitHubProvenance(repository)
        default_branch = provenance.default_branch(github_repository)
        main_ci_succeeded = provenance.main_ci_succeeded(github_repository, commit)

    return evaluate_preflight(
        event_name=event_name,
        ref_name=ref_name,
        commit=commit,
        package_version=package_version,
        tag_commit=tag_commit,
        main_contains_commit=main_contains_commit,
        workflow_files_match_main=workflow_files_match_main,
        default_branch=default_branch,
        main_ci_succeeded=main_ci_succeeded,
    )


def workflow_outputs(result: PreflightResult) -> dict[str, str]:
    """Return validated preflight values for GitHub Actions."""

    matrix = {"include": [target.matrix_entry() for target in TARGETS]}
    return {
        "version": result.version,
        "tag": result.tag,
        "commit": result.commit,
        "publish": str(result.publish).lower(),
        "matrix": json.dumps(matrix, separators=(",", ":"), sort_keys=True),
    }


def append_github_outputs(path: Path, outputs: dict[str, str]) -> None:
    """Append single-line validated values to a GitHub output file."""

    for name, value in outputs.items():
        if "\n" in value or "\r" in value:
            raise ReleaseError(
                f"workflow output {name!r} unexpectedly contains a newline"
            )
    with path.open("a", encoding="utf-8", newline="\n") as output_file:
        for name, value in outputs.items():
            output_file.write(f"{name}={value}\n")


def rustc_runner_evidence(
    repository: Path,
    *,
    runner_arch: str,
    expected_runner_arch: str,
    expected_host: str,
    target: str,
) -> str:
    """Validate runner architecture and rustc host against the fixed matrix row."""

    target_for(target)
    if runner_arch != expected_runner_arch:
        raise ReleaseError(
            f"RUNNER_ARCH is {runner_arch!r}; expected {expected_runner_arch!r}"
        )
    verbose = run_checked(("rustc", "-vV"), cwd=repository)
    try:
        evidence = verbose.stdout.decode()
    except UnicodeDecodeError as error:
        raise ReleaseError("rustc -vV output was not valid UTF-8") from error
    hosts = [
        line.removeprefix("host: ")
        for line in evidence.splitlines()
        if line.startswith("host: ")
    ]
    if hosts != [expected_host]:
        raise ReleaseError(
            f"rustc host evidence is {hosts!r}; expected [{expected_host!r}]"
        )
    if expected_host != target:
        raise ReleaseError(
            f"target {target!r} is not the native host {expected_host!r}; a cross-compiled "
            "result is not native release verification"
        )
    return evidence


def linked_packages(repository: Path, target: str) -> list[dict[str, object]]:
    """Return the locked non-dev dependency graph that produces one target's binary.

    Build dependencies are excluded because they are not linked into the shipped
    executable. Everything reachable through a normal edge is included, including
    procedural macros: over-disclosure is safe, and deciding what a linker really kept
    would need evidence this script does not have.
    """

    target_for(target)
    completed = run_checked(
        (
            "cargo",
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            target,
        ),
        cwd=repository,
    )
    try:
        metadata = json.loads(completed.stdout)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ReleaseError("cargo metadata did not return valid UTF-8 JSON") from error

    resolve = metadata.get("resolve")
    if not isinstance(resolve, dict):
        raise ReleaseError("cargo metadata carried no resolve graph")
    root = resolve.get("root")
    if not isinstance(root, str):
        raise ReleaseError("cargo metadata carried no resolve root")
    nodes = {
        node["id"]: node
        for node in resolve.get("nodes", [])
        if isinstance(node, dict) and isinstance(node.get("id"), str)
    }
    packages = {
        package["id"]: package
        for package in metadata.get("packages", [])
        if isinstance(package, dict) and isinstance(package.get("id"), str)
    }
    if root not in nodes:
        raise ReleaseError("cargo metadata resolve root is not in the node set")

    reached: set[str] = set()
    pending = [root]
    while pending:
        current = pending.pop()
        if current in reached:
            continue
        reached.add(current)
        for dependency in nodes[current].get("deps", []):
            kinds = {
                kind.get("kind") for kind in dependency.get("dep_kinds", [])
            }
            if None in kinds:
                pending.append(dependency["pkg"])

    reached.discard(root)
    missing = sorted(identifier for identifier in reached if identifier not in packages)
    if missing:
        raise ReleaseError(f"cargo metadata omitted resolved packages: {missing}")
    return sorted(
        (packages[identifier] for identifier in reached),
        key=lambda package: (str(package.get("name")), str(package.get("version"))),
    )


def license_texts(package: dict[str, object]) -> list[tuple[str, str]]:
    """Return the licence texts a package carries, at its root or in a vendored tree."""

    name = str(package.get("name"))
    manifest_path = package.get("manifest_path")
    if not isinstance(manifest_path, str):
        raise ReleaseError(f"package {name!r} has no manifest path")
    root = Path(manifest_path).parent
    if not root.is_dir():
        raise ReleaseError(f"package {name!r} source is not present at {root}")

    texts: list[tuple[str, str]] = []
    for candidate in sorted(root.iterdir(), key=lambda entry: entry.name):
        if not candidate.is_file():
            continue
        upper = candidate.name.upper()
        if not upper.startswith(LICENSE_TEXT_PREFIXES):
            continue
        if candidate.name.endswith(LICENSE_TEXT_EXCLUDED_SUFFIXES):
            continue
        texts.append((candidate.name, read_license_text(candidate)))

    vendored = VENDORED_LICENSES.get(name)
    if vendored is not None:
        for relative in vendored.paths:
            path = root / relative
            if not path.is_file():
                raise ReleaseError(
                    f"package {name!r} is recorded as vendoring {relative}, which is absent"
                )
            texts.append((relative, read_license_text(path)))

    if not texts:
        raise ReleaseError(
            f"package {name!r} has no licence text at its crate root and no recorded "
            "vendored source; a release cannot ship an obligation it cannot reproduce"
        )
    return texts


def read_license_text(path: Path) -> str:
    """Read one licence file as text with normalized line endings."""

    try:
        return path.read_text(encoding="utf-8").replace("\r\n", "\n")
    except (OSError, UnicodeError) as error:
        raise ReleaseError(f"cannot read licence text {path}: {error}") from error


def package_origin(package: dict[str, object]) -> str:
    """Return a human-readable origin for one locked package."""

    source = package.get("source")
    if isinstance(source, str) and source:
        return source
    return "local path dependency"


def third_party_licenses(
    repository: Path, *, target: str, version: str, commit: str
) -> str:
    """Render the third-party licence bundle for one target's locked graph."""

    validate_stable_version(version)
    validate_commit(commit)
    triple = target_for(target).triple
    packages = linked_packages(repository, triple)

    lines = [
        f"{PRODUCT_NAME} {version} — third-party licences",
        f"target: {triple}",
        f"source commit: {commit}",
        "",
        "Every crate in the locked, non-development dependency graph that produces this",
        "binary is listed below with the licence text it distributes. Build-only",
        f"dependencies are excluded. {PRODUCT_NAME} itself is licensed under Apache-2.0;",
        f"see {LICENSE_FILE}. Obligations that crate metadata cannot express are recorded",
        f"in {NOTICE_FILE}, which ships beside this file.",
        "",
        f"{len(packages)} third-party package(s).",
    ]

    for package in packages:
        name = str(package.get("name"))
        package_version = str(package.get("version"))
        expression = package.get("license")
        declared = expression if isinstance(expression, str) and expression else "not declared in metadata"
        lines.extend(
            [
                "",
                "=" * 78,
                f"{name} {package_version}",
                f"SPDX expression: {declared}",
                f"Source: {package_origin(package)}",
            ]
        )
        vendored = VENDORED_LICENSES.get(name)
        if vendored is not None:
            lines.append(f"Note: {vendored.reason}")
        for filename, text in license_texts(package):
            lines.extend(["", f"--- {filename} ---", text.rstrip("\n")])

    return "\n".join(lines) + "\n"


def require_regular_file(path: Path, label: str) -> os.stat_result:
    """Return lstat evidence for a regular, non-link file."""

    try:
        metadata = path.lstat()
    except OSError as error:
        raise ReleaseError(f"cannot inspect {label} at {path}: {error}") from error
    if not stat.S_ISREG(metadata.st_mode):
        raise ReleaseError(f"{label} is not a regular file: {path}")
    return metadata


def windows_imported_libraries(executable: Path) -> tuple[str, ...]:
    """Return the DLL names in a PE import table, in file order.

    Parsed here rather than shelled out to `dumpbin`, which lives inside a Visual Studio
    developer environment this workflow does not otherwise enter, and which cannot be run
    at all by the offline validation job that tests this function.
    """

    try:
        data = executable.read_bytes()
    except OSError as error:
        raise ReleaseError(f"cannot read {executable}: {error}") from error

    if len(data) < 0x40 or data[:2] != b"MZ":
        raise ReleaseError(f"{executable} is not a PE image")
    (pe_offset,) = struct.unpack_from("<I", data, 0x3C)
    if pe_offset + 24 > len(data) or data[pe_offset : pe_offset + 4] != b"PE\0\0":
        raise ReleaseError(f"{executable} has no PE header")
    coff = pe_offset + 4
    # Machine, NumberOfSections, TimeDateStamp, PointerToSymbolTable, NumberOfSymbols,
    # SizeOfOptionalHeader, Characteristics.
    _, section_count, _, _, _, optional_size, _ = struct.unpack_from(
        "<HHIIIHH", data, coff
    )
    optional = coff + 20
    if optional + optional_size > len(data):
        raise ReleaseError(f"{executable} has a truncated optional header")
    (magic,) = struct.unpack_from("<H", data, optional)
    if magic != 0x20B:
        raise ReleaseError(f"{executable} is not a PE32+ image (magic {magic:#x})")
    (directory_count,) = struct.unpack_from("<I", data, optional + 108)
    if directory_count < 2:
        return ()
    import_rva, import_size = struct.unpack_from("<II", data, optional + 112 + 8)
    if import_rva == 0 or import_size == 0:
        return ()

    sections = []
    section_table = optional + optional_size
    for index in range(section_count):
        entry = section_table + index * 40
        if entry + 40 > len(data):
            raise ReleaseError(f"{executable} has a truncated section table")
        virtual_size, virtual_address, raw_size, raw_pointer = struct.unpack_from(
            "<IIII", data, entry + 8
        )
        sections.append((virtual_address, max(virtual_size, raw_size), raw_pointer, raw_size))

    def offset_for(rva: int) -> int:
        for virtual_address, span, raw_pointer, raw_size in sections:
            if virtual_address <= rva < virtual_address + span:
                offset = raw_pointer + (rva - virtual_address)
                if offset >= len(data) or rva - virtual_address >= raw_size:
                    raise ReleaseError(f"{executable} maps RVA {rva:#x} outside its file")
                return offset
        raise ReleaseError(f"{executable} has no section containing RVA {rva:#x}")

    names: list[str] = []
    descriptor = offset_for(import_rva)
    while True:
        if descriptor + 20 > len(data):
            raise ReleaseError(f"{executable} has a truncated import directory")
        fields = struct.unpack_from("<IIIII", data, descriptor)
        if not any(fields):
            break
        name_offset = offset_for(fields[3])
        end = data.find(b"\0", name_offset)
        if end < 0:
            raise ReleaseError(f"{executable} has an unterminated import name")
        names.append(data[name_offset:end].decode("ascii", errors="replace"))
        descriptor += 20
    return tuple(names)


def macos_loaded_dylibs(executable: Path) -> tuple[str, ...]:
    """Return the dylib paths a thin 64-bit Mach-O image loads, in load-command order."""

    try:
        data = executable.read_bytes()
    except OSError as error:
        raise ReleaseError(f"cannot read {executable}: {error}") from error

    if len(data) < 32:
        raise ReleaseError(f"{executable} is too small to be a Mach-O image")
    (magic,) = struct.unpack_from("<I", data, 0)
    if magic != 0xFEEDFACF:
        raise ReleaseError(
            f"{executable} is not a thin little-endian 64-bit Mach-O image "
            f"(magic {magic:#x})"
        )
    command_count, command_size = struct.unpack_from("<II", data, 16)
    if 32 + command_size > len(data):
        raise ReleaseError(f"{executable} has a truncated load-command region")

    paths: list[str] = []
    offset = 32
    for _ in range(command_count):
        command, size = struct.unpack_from("<II", data, offset)
        if size < 8 or offset + size > len(data):
            raise ReleaseError(f"{executable} has a malformed load command")
        # LC_LOAD_DYLIB, LC_LOAD_WEAK_DYLIB, LC_REEXPORT_DYLIB.
        if command in (0x0C, 0x80000018, 0x8000001F):
            (name_offset,) = struct.unpack_from("<I", data, offset + 8)
            if not 24 <= name_offset < size:
                raise ReleaseError(f"{executable} has a malformed dylib command")
            raw = data[offset + name_offset : offset + size]
            paths.append(raw.split(b"\0", 1)[0].decode("utf-8", errors="replace"))
        offset += size
    return tuple(paths)


def inspect_linkage(executable: Path, target: Target) -> tuple[str, ...]:
    """Reject a release binary that needs anything but its operating system."""

    require_regular_file(executable, "release executable")
    if target.triple == "x86_64-pc-windows-msvc":
        imports = windows_imported_libraries(executable)
        if not imports:
            raise ReleaseError(
                f"{executable} imports nothing at all; that is not a linked program"
            )
        for name in imports:
            lowered = name.lower()
            if not lowered.endswith(".dll") or "/" in name or "\\" in name:
                raise ReleaseError(f"{executable} imports a malformed library {name!r}")
            if lowered.startswith(WINDOWS_FORBIDDEN_IMPORT_PREFIXES):
                raise ReleaseError(
                    f"{executable} imports {name}, so it needs a Visual C++ "
                    "redistributable; the release build requires a static CRT"
                )
        return imports

    dylibs = macos_loaded_dylibs(executable)
    if not dylibs:
        raise ReleaseError(
            f"{executable} loads no dylib at all; that is not a linked program"
        )
    for path in dylibs:
        if not path.startswith(MACOS_ALLOWED_DYLIB_ROOTS):
            raise ReleaseError(
                f"{executable} loads {path}, which is not a macOS system library; a "
                "build-machine path will not exist on a user's Mac"
            )
    return dylibs


def stage_package(
    repository: Path,
    binary_directory: Path,
    third_party_path: Path,
    stage_root: Path,
    target: Target,
    metadata: bytes,
) -> None:
    """Stage only the exact package members under one top-level directory."""

    stage_root.mkdir()
    executable = executable_name(target)
    source = binary_directory / executable
    source_metadata = require_regular_file(source, f"{executable} binary")
    if target.triple == "aarch64-apple-darwin" and source_metadata.st_mode & 0o111 == 0:
        raise ReleaseError(f"macOS release binary is not executable: {source}")
    inspect_linkage(source, target)
    destination = stage_root / executable
    shutil.copyfile(source, destination)
    destination.chmod(0o755)

    for name, path in (
        (LICENSE_FILE, repository / LICENSE_FILE),
        (NOTICE_FILE, repository / NOTICE_FILE),
        (THIRD_PARTY_FILE, third_party_path),
    ):
        require_regular_file(path, name)
        if path.stat().st_size == 0:
            raise ReleaseError(f"{name} is empty; a release cannot ship a blank notice")
        copied = stage_root / name
        shutil.copyfile(path, copied)
        copied.chmod(0o644)

    version_path = stage_root / VERSION_FILE
    version_path.write_bytes(metadata)
    version_path.chmod(0o644)


def zip_info(name: str, mode: int, *, directory: bool) -> zipfile.ZipInfo:
    """Create normalized ZIP metadata for one member."""

    info = zipfile.ZipInfo(name, ZIP_TIMESTAMP)
    info.create_system = 3
    info.compress_type = zipfile.ZIP_DEFLATED
    file_type = stat.S_IFDIR if directory else stat.S_IFREG
    info.external_attr = ((file_type | mode) << 16) | (0x10 if directory else 0)
    return info


def write_zip(stage_root: Path, archive: Path, tag: str, target: Target) -> None:
    """Write one deterministic Windows ZIP package."""

    root_name = asset_stem(tag, target)
    with zipfile.ZipFile(
        archive,
        mode="w",
        compression=zipfile.ZIP_DEFLATED,
        compresslevel=9,
        strict_timestamps=True,
    ) as output:
        output.writestr(zip_info(f"{root_name}/", 0o755, directory=True), b"")
        for filename in expected_file_names(target):
            mode = 0o755 if filename == executable_name(target) else 0o644
            info = zip_info(f"{root_name}/{filename}", mode, directory=False)
            with (stage_root / filename).open("rb") as source, output.open(
                info, "w"
            ) as destination:
                shutil.copyfileobj(source, destination, length=1024 * 1024)


def tar_info(name: str, mode: int, *, directory: bool, size: int = 0) -> tarfile.TarInfo:
    """Create normalized tar metadata for one member."""

    info = tarfile.TarInfo(name)
    info.type = tarfile.DIRTYPE if directory else tarfile.REGTYPE
    info.mode = mode
    info.size = size
    info.mtime = 0
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    return info


def write_tar_gz(stage_root: Path, archive: Path, tag: str, target: Target) -> None:
    """Write one deterministic Apple Silicon tar.gz package."""

    root_name = asset_stem(tag, target)
    with archive.open("wb") as raw_output:
        with gzip.GzipFile(
            filename="", mode="wb", fileobj=raw_output, compresslevel=9, mtime=0
        ) as compressed:
            with tarfile.open(
                fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT
            ) as output:
                output.addfile(tar_info(f"{root_name}/", 0o755, directory=True))
                for filename in expected_file_names(target):
                    source_path = stage_root / filename
                    source_metadata = require_regular_file(source_path, filename)
                    mode = 0o755 if filename == executable_name(target) else 0o644
                    info = tar_info(
                        f"{root_name}/{filename}",
                        mode,
                        directory=False,
                        size=source_metadata.st_size,
                    )
                    with source_path.open("rb") as source:
                        output.addfile(info, source)


def validate_member_name(name: str) -> None:
    """Reject absolute, traversal, backslash, or malformed archive member names."""

    if "\\" in name:
        raise ReleaseError(f"archive member uses a backslash: {name!r}")
    path = PurePosixPath(name)
    if path.is_absolute() or any(part in ("", ".", "..") for part in path.parts):
        raise ReleaseError(f"archive member is not a safe relative path: {name!r}")


def inspect_zip(
    archive: Path, target: Target, tag: str, expected_metadata: bytes
) -> None:
    """Verify ZIP layout, kinds, timestamps, permissions, and metadata."""

    try:
        with zipfile.ZipFile(archive) as package:
            members = package.infolist()
            names = tuple(member.filename for member in members)
            if len(names) != len(set(names)):
                raise ReleaseError(f"ZIP contains duplicate members: {archive}")
            if names != expected_member_names(tag, target):
                raise ReleaseError(
                    f"ZIP member set/order is {names!r}; expected "
                    f"{expected_member_names(tag, target)!r}"
                )
            for index, member in enumerate(members):
                validate_member_name(member.filename)
                expected_directory = index == 0
                if member.is_dir() != expected_directory:
                    raise ReleaseError(
                        f"ZIP member has unexpected kind: {member.filename}"
                    )
                if member.date_time != ZIP_TIMESTAMP:
                    raise ReleaseError(
                        f"ZIP member has non-normalized time: {member.filename}"
                    )
                mode = (member.external_attr >> 16) & 0o777
                filename = PurePosixPath(member.filename).name
                expected_mode = (
                    0o755
                    if expected_directory or filename == executable_name(target)
                    else 0o644
                )
                if mode != expected_mode:
                    raise ReleaseError(
                        f"ZIP member {member.filename} has mode {mode:o}; "
                        f"expected {expected_mode:o}"
                    )
            root = asset_stem(tag, target)
            if package.read(f"{root}/{VERSION_FILE}") != expected_metadata:
                raise ReleaseError(
                    "ZIP VERSION metadata does not match validated release values"
                )
            for filename in expected_file_names(target):
                if package.getinfo(f"{root}/{filename}").file_size == 0:
                    raise ReleaseError(f"ZIP member is empty: {filename}")
    except (OSError, zipfile.BadZipFile) as error:
        raise ReleaseError(f"cannot inspect ZIP archive {archive}: {error}") from error


def inspect_tar_gz(
    archive: Path, target: Target, tag: str, expected_metadata: bytes
) -> None:
    """Verify tar.gz layout, kinds, timestamps, permissions, and metadata."""

    try:
        with archive.open("rb") as compressed:
            header = compressed.read(10)
        if (
            len(header) != 10
            or header[:3] != b"\x1f\x8b\x08"
            or header[4:8] != b"\0\0\0\0"
        ):
            raise ReleaseError(f"gzip header is not normalized: {archive}")
        with tarfile.open(archive, mode="r:gz") as package:
            members = package.getmembers()
            names = tuple(
                f"{member.name.rstrip('/')}/" if member.isdir() else member.name
                for member in members
            )
            if len(names) != len(set(names)):
                raise ReleaseError(f"tar archive contains duplicate members: {archive}")
            if names != expected_member_names(tag, target):
                raise ReleaseError(
                    f"tar member set/order is {names!r}; expected "
                    f"{expected_member_names(tag, target)!r}"
                )
            for index, member in enumerate(members):
                validate_member_name(member.name)
                expected_directory = index == 0
                if member.isdir() != expected_directory:
                    raise ReleaseError(f"tar member has unexpected kind: {member.name}")
                if not expected_directory and not member.isreg():
                    raise ReleaseError(
                        f"tar member is not a regular file: {member.name}"
                    )
                filename = PurePosixPath(member.name).name
                expected_mode = (
                    0o755
                    if expected_directory or filename == executable_name(target)
                    else 0o644
                )
                if member.mode != expected_mode:
                    raise ReleaseError(
                        f"tar member {member.name} has mode {member.mode:o}; "
                        f"expected {expected_mode:o}"
                    )
                if (
                    member.mtime != 0
                    or member.uid != 0
                    or member.gid != 0
                    or member.uname != ""
                    or member.gname != ""
                ):
                    raise ReleaseError(f"tar metadata is not normalized: {member.name}")
            root = asset_stem(tag, target)
            metadata_member = package.extractfile(f"{root}/{VERSION_FILE}")
            if metadata_member is None or metadata_member.read() != expected_metadata:
                raise ReleaseError(
                    "tar VERSION metadata does not match validated release values"
                )
            for filename in expected_file_names(target):
                if package.getmember(f"{root}/{filename}").size == 0:
                    raise ReleaseError(f"tar member is empty: {filename}")
    except (OSError, tarfile.TarError) as error:
        raise ReleaseError(f"cannot inspect tar.gz archive {archive}: {error}") from error


def inspect_archive(
    archive: Path, *, target: Target, version: str, tag: str, commit: str
) -> None:
    """Verify one archive against the complete deterministic package contract."""

    expected_name = asset_name(tag, target)
    if archive.name != expected_name:
        raise ReleaseError(
            f"archive is named {archive.name!r}; expected {expected_name!r}"
        )
    require_regular_file(archive, "release archive")
    metadata = version_metadata(version, tag, target, commit)
    if target.extension == ".zip":
        inspect_zip(archive, target, tag, metadata)
    elif target.extension == ".tar.gz":
        inspect_tar_gz(archive, target, tag, metadata)
    else:
        raise ReleaseError(f"unsupported archive extension {target.extension!r}")


def package_release(
    repository: Path,
    binary_directory: Path,
    third_party_path: Path,
    output_directory: Path,
    *,
    target: Target,
    version: str,
    tag: str,
    commit: str,
) -> Path:
    """Stage, package, and reinspect one target archive."""

    repository = repository.resolve()
    binary_directory = binary_directory.resolve()
    third_party_path = third_party_path.resolve()
    output_directory.mkdir(parents=True, exist_ok=True)
    output_directory = output_directory.resolve()
    metadata = version_metadata(version, tag, target, commit)
    final_archive = output_directory / asset_name(tag, target)

    with tempfile.TemporaryDirectory(
        prefix=".release-stage-", dir=output_directory
    ) as temporary:
        temporary_path = Path(temporary)
        stage_root = temporary_path / asset_stem(tag, target)
        stage_package(
            repository,
            binary_directory,
            third_party_path,
            stage_root,
            target,
            metadata,
        )
        temporary_archive = temporary_path / final_archive.name
        if target.extension == ".zip":
            write_zip(stage_root, temporary_archive, tag, target)
        else:
            write_tar_gz(stage_root, temporary_archive, tag, target)
        inspect_archive(
            temporary_archive, target=target, version=version, tag=tag, commit=commit
        )
        os.replace(temporary_archive, final_archive)

    inspect_archive(
        final_archive, target=target, version=version, tag=tag, commit=commit
    )
    return final_archive


def sha256_file(path: Path) -> str:
    """Return a streaming SHA-256 digest for one regular file."""

    require_regular_file(path, path.name)
    digest = hashlib.sha256()
    with path.open("rb") as input_file:
        while chunk := input_file.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def regular_files_below(root: Path) -> list[Path]:
    """List regular files below *root* without following links."""

    if not root.is_dir():
        raise ReleaseError(f"release input directory does not exist: {root}")
    files: list[Path] = []
    for directory, directory_names, file_names in os.walk(root, followlinks=False):
        directory_path = Path(directory)
        for name in directory_names:
            candidate = directory_path / name
            if candidate.is_symlink():
                raise ReleaseError(
                    f"release input contains a directory link: {candidate}"
                )
        for name in file_names:
            candidate = directory_path / name
            require_regular_file(candidate, "release input")
            files.append(candidate)
    return files


def checksum_text(archives: Sequence[Path]) -> str:
    """Return deterministic GNU-compatible SHA-256 checksum lines."""

    ordered = sorted(archives, key=lambda archive: archive.name)
    return "".join(f"{sha256_file(archive)}  {archive.name}\n" for archive in ordered)


def parse_checksum_file(path: Path) -> dict[str, str]:
    """Parse the exact checksum syntax produced by this release helper."""

    require_regular_file(path, CHECKSUM_FILE)
    try:
        lines = path.read_text(encoding="ascii").splitlines()
    except (OSError, UnicodeError) as error:
        raise ReleaseError(f"cannot read {CHECKSUM_FILE}: {error}") from error
    checksums: dict[str, str] = {}
    for line in lines:
        match = re.fullmatch(r"([0-9a-f]{64})  ([^/\\]+)", line)
        if match is None:
            raise ReleaseError(f"invalid {CHECKSUM_FILE} line: {line!r}")
        digest, filename = match.groups()
        if filename in checksums:
            raise ReleaseError(f"duplicate {CHECKSUM_FILE} entry: {filename}")
        checksums[filename] = digest
    if list(checksums) != sorted(checksums):
        raise ReleaseError(f"{CHECKSUM_FILE} entries are not sorted")
    return checksums


def aggregate_release(
    input_directory: Path,
    output_directory: Path,
    *,
    version: str,
    tag: str,
    commit: str,
) -> tuple[Path, ...]:
    """Require both valid archives and assemble the verified asset set."""

    expected_names = set(expected_archive_names(tag))
    discovered: dict[str, Path] = {}
    for path in regular_files_below(input_directory):
        if path.name not in expected_names:
            raise ReleaseError(f"unexpected release artifact: {path}")
        if path.name in discovered:
            raise ReleaseError(f"duplicate release artifact named {path.name!r}")
        discovered[path.name] = path
    missing = expected_names.difference(discovered)
    if missing:
        raise ReleaseError(f"missing release artifacts: {', '.join(sorted(missing))}")

    for target in TARGETS:
        inspect_archive(
            discovered[asset_name(tag, target)],
            target=target,
            version=version,
            tag=tag,
            commit=commit,
        )

    if output_directory.exists() and any(output_directory.iterdir()):
        raise ReleaseError(f"release output directory is not empty: {output_directory}")
    output_directory.mkdir(parents=True, exist_ok=True)
    output_archives: list[Path] = []
    for name in sorted(discovered):
        destination = output_directory / name
        shutil.copyfile(discovered[name], destination)
        output_archives.append(destination)
    (output_directory / CHECKSUM_FILE).write_text(
        checksum_text(output_archives), encoding="ascii", newline="\n"
    )
    verify_release_set(output_directory, version=version, tag=tag, commit=commit)
    return tuple(output_archives)


def verify_release_set(
    directory: Path, *, version: str, tag: str, commit: str
) -> None:
    """Verify the exact three-file release set and every local checksum."""

    expected_archives = set(expected_archive_names(tag))
    expected_files = expected_archives | {CHECKSUM_FILE}
    files = regular_files_below(directory)
    relative_names = []
    for path in files:
        relative = path.relative_to(directory)
        if len(relative.parts) != 1:
            raise ReleaseError(f"release asset is not at the workspace root: {relative}")
        relative_names.append(path.name)
    if set(relative_names) != expected_files or len(relative_names) != len(
        expected_files
    ):
        raise ReleaseError(
            f"release asset set is {sorted(relative_names)!r}; "
            f"expected {sorted(expected_files)!r}"
        )

    for target in TARGETS:
        inspect_archive(
            directory / asset_name(tag, target),
            target=target,
            version=version,
            tag=tag,
            commit=commit,
        )
    checksums = parse_checksum_file(directory / CHECKSUM_FILE)
    if set(checksums) != expected_archives:
        raise ReleaseError(
            f"{CHECKSUM_FILE} covers {sorted(checksums)!r}; "
            f"expected {sorted(expected_archives)!r}"
        )
    for filename, expected_digest in checksums.items():
        actual_digest = sha256_file(directory / filename)
        if actual_digest != expected_digest:
            raise ReleaseError(
                f"SHA-256 mismatch for {filename}: expected {expected_digest}, "
                f"got {actual_digest}"
            )


def argument_parser() -> argparse.ArgumentParser:
    """Build the release-helper command-line parser."""

    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    preflight_parser = subparsers.add_parser("preflight")
    preflight_parser.add_argument("--repository", type=Path, default=Path.cwd())
    preflight_parser.add_argument("--event-name", required=True)
    preflight_parser.add_argument("--ref-name", required=True)
    preflight_parser.add_argument("--event-sha", required=True)
    preflight_parser.add_argument("--github-repository", required=True)
    preflight_parser.add_argument("--github-output", type=Path, required=True)

    source_parser = subparsers.add_parser("verify-publication-source")
    source_parser.add_argument("--repository", type=Path, default=Path.cwd())
    source_parser.add_argument("--commit", required=True)
    source_parser.add_argument("--github-repository", required=True)

    runner_parser = subparsers.add_parser("verify-runner")
    runner_parser.add_argument("--repository", type=Path, default=Path.cwd())
    runner_parser.add_argument("--runner-arch", required=True)
    runner_parser.add_argument("--expected-runner-arch", required=True)
    runner_parser.add_argument("--expected-host", required=True)
    runner_parser.add_argument("--target", required=True)

    licenses_parser = subparsers.add_parser("licenses")
    licenses_parser.add_argument("--repository", type=Path, default=Path.cwd())
    licenses_parser.add_argument("--target", required=True)
    licenses_parser.add_argument("--version", required=True)
    licenses_parser.add_argument("--commit", required=True)
    licenses_parser.add_argument("--output", type=Path, required=True)

    linkage_parser = subparsers.add_parser("inspect-linkage")
    linkage_parser.add_argument("--executable", type=Path, required=True)
    linkage_parser.add_argument("--target", required=True)

    package_parser = subparsers.add_parser("package")
    package_parser.add_argument("--repository", type=Path, default=Path.cwd())
    package_parser.add_argument("--binary-directory", type=Path, required=True)
    package_parser.add_argument("--third-party-licenses", type=Path, required=True)
    package_parser.add_argument("--output-directory", type=Path, required=True)
    package_parser.add_argument("--target", required=True)
    package_parser.add_argument("--version", required=True)
    package_parser.add_argument("--tag", required=True)
    package_parser.add_argument("--commit", required=True)

    aggregate_parser = subparsers.add_parser("aggregate")
    aggregate_parser.add_argument("--input-directory", type=Path, required=True)
    aggregate_parser.add_argument("--output-directory", type=Path, required=True)
    aggregate_parser.add_argument("--version", required=True)
    aggregate_parser.add_argument("--tag", required=True)
    aggregate_parser.add_argument("--commit", required=True)

    verify_parser = subparsers.add_parser("verify-set")
    verify_parser.add_argument("--directory", type=Path, required=True)
    verify_parser.add_argument("--version", required=True)
    verify_parser.add_argument("--tag", required=True)
    verify_parser.add_argument("--commit", required=True)

    return parser


def run(arguments: Sequence[str]) -> int:
    """Run one release-helper subcommand."""

    options = argument_parser().parse_args(arguments)

    if options.command == "preflight":
        result = preflight(
            options.repository,
            event_name=options.event_name,
            ref_name=options.ref_name,
            event_sha=options.event_sha,
            github_repository=options.github_repository,
        )
        append_github_outputs(options.github_output, workflow_outputs(result))
        print(
            f"Validated {result.tag} at {result.commit}; "
            f"publish={str(result.publish).lower()}"
        )
        return 0

    if options.command == "verify-publication-source":
        verify_publication_source(
            options.repository,
            options.commit,
            github_repository=options.github_repository,
        )
        print(f"Validated current main publication compatibility for {options.commit}")
        return 0

    if options.command == "verify-runner":
        evidence = rustc_runner_evidence(
            options.repository,
            runner_arch=options.runner_arch,
            expected_runner_arch=options.expected_runner_arch,
            expected_host=options.expected_host,
            target=options.target,
        )
        print(evidence, end="" if evidence.endswith("\n") else "\n")
        print(f"Validated native runner target {options.target}")
        return 0

    if options.command == "licenses":
        text = third_party_licenses(
            options.repository,
            target=options.target,
            version=options.version,
            commit=options.commit,
        )
        options.output.parent.mkdir(parents=True, exist_ok=True)
        options.output.write_text(text, encoding="utf-8", newline="\n")
        print(f"Wrote {options.output} ({len(text.encode())} bytes)")
        return 0

    if options.command == "inspect-linkage":
        target = target_for(options.target)
        libraries = inspect_linkage(options.executable, target)
        for library in libraries:
            print(f"  {library}")
        print(f"Validated {len(libraries)} system dependency reference(s)")
        return 0

    if options.command == "package":
        target = target_for(options.target)
        archive = package_release(
            options.repository,
            options.binary_directory,
            options.third_party_licenses,
            options.output_directory,
            target=target,
            version=options.version,
            tag=options.tag,
            commit=options.commit,
        )
        print(archive)
        return 0

    if options.command == "aggregate":
        archives = aggregate_release(
            options.input_directory,
            options.output_directory,
            version=options.version,
            tag=options.tag,
            commit=options.commit,
        )
        print(f"Verified {len(archives)} release archives and {CHECKSUM_FILE}")
        return 0

    if options.command == "verify-set":
        verify_release_set(
            options.directory,
            version=options.version,
            tag=options.tag,
            commit=options.commit,
        )
        print(f"Verified complete release set in {options.directory}")
        return 0

    raise ReleaseError(f"unhandled release command {options.command!r}")


def main(arguments: Sequence[str] | None = None) -> int:
    """Convert release invariant failures into a stable nonzero status."""

    try:
        return run(sys.argv[1:] if arguments is None else arguments)
    except (OSError, ReleaseError, UnicodeError, json.JSONDecodeError) as error:
        print(f"release validation failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
