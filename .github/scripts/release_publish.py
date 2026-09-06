#!/usr/bin/env python3
"""Publish one verified comic-auto-resize release without creating or moving its tag.

The asymmetry is the point. Anything unexpected — a foreign draft, a completed asset whose
bytes differ, a tag that moved — stops for review. There is no operation here that deletes
a published asset, replaces one, or writes a ref; a defective binary is corrected by a new
reviewed version, not by rewriting bytes somebody has already downloaded.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any, Sequence
from urllib.parse import quote

import release as release_assets

API_VERSION = release_assets.API_VERSION
WORKFLOW_MARKER_VERSION = 1


class PublishError(RuntimeError):
    """Remote release state cannot be changed safely."""


def release_marker(tag: str, commit: str) -> str:
    """Return the stable ownership marker for one immutable release identity."""

    release_assets.stable_version_from_tag(tag)
    release_assets.validate_commit(commit)
    return (
        f"<!-- {release_assets.PRODUCT_NAME}-release-workflow:v{WORKFLOW_MARKER_VERSION} "
        f"tag={tag} commit={commit} -->"
    )


def release_body(tag: str, commit: str, run_url: str) -> str:
    """Return the draft marker and actionable retry evidence."""

    if not run_url.startswith("https://github.com/") or any(
        character.isspace() for character in run_url
    ):
        raise PublishError(f"workflow run URL is not a trusted GitHub URL: {run_url!r}")
    return (
        f"{release_marker(tag, commit)}\n\n"
        f"This draft is owned by the {release_assets.PRODUCT_NAME} release workflow.\n\n"
        f"Initial workflow run: {run_url}\n\n"
        "If asset upload is interrupted, rerun the workflow for this same tag. "
        "Do not move or recreate the tag."
    )


def expected_local_assets(
    directory: Path, *, version: str, tag: str, commit: str
) -> dict[str, Path]:
    """Verify the local release boundary before any GitHub API interaction."""

    release_assets.verify_release_set(
        directory, version=version, tag=tag, commit=commit
    )
    names = (*release_assets.expected_archive_names(tag), release_assets.CHECKSUM_FILE)
    return {name: directory / name for name in names}


def flatten_release_pages(value: Any) -> list[dict[str, Any]]:
    """Flatten `gh api --paginate --slurp` release pages."""

    if not isinstance(value, list):
        raise PublishError("GitHub release listing was not a JSON array")
    releases: list[dict[str, Any]] = []
    for page in value:
        if not isinstance(page, list):
            raise PublishError("GitHub release listing page was not a JSON array")
        for item in page:
            if not isinstance(item, dict):
                raise PublishError("GitHub release listing contained a non-object")
            releases.append(item)
    return releases


def find_release(releases: Sequence[dict[str, Any]], tag: str) -> dict[str, Any] | None:
    """Find at most one release, including drafts, for *tag*."""

    matches = [candidate for candidate in releases if candidate.get("tag_name") == tag]
    if len(matches) > 1:
        raise PublishError(f"multiple GitHub Releases already use tag {tag!r}")
    return matches[0] if matches else None


def require_release_identity(
    candidate: dict[str, Any], *, tag: str, commit: str
) -> None:
    """Require workflow ownership and immutable tag identity on a remote release."""

    if candidate.get("tag_name") != tag:
        raise PublishError(
            f"release tag is {candidate.get('tag_name')!r}; expected {tag!r}"
        )
    if candidate.get("name") != tag:
        raise PublishError(
            f"release title is {candidate.get('name')!r}; expected exact tag {tag!r}"
        )
    if candidate.get("prerelease") is not False:
        raise PublishError("stable release is unexpectedly marked as a prerelease")
    body = candidate.get("body")
    marker = release_marker(tag, commit)
    if not isinstance(body, str) or marker not in body:
        raise PublishError(
            f"release for {tag} does not carry the matching workflow ownership marker"
        )


def asset_map(candidate: dict[str, Any]) -> dict[str, dict[str, Any]]:
    """Index release assets by unique filename."""

    assets = candidate.get("assets")
    if not isinstance(assets, list):
        raise PublishError("release assets are missing or not a JSON array")
    indexed: dict[str, dict[str, Any]] = {}
    for asset in assets:
        if not isinstance(asset, dict) or not isinstance(asset.get("name"), str):
            raise PublishError("release asset metadata is malformed")
        name = asset["name"]
        if name in indexed:
            raise PublishError(f"release contains duplicate asset name {name!r}")
        indexed[name] = asset
    return indexed


def require_uploaded_asset_matches(asset: dict[str, Any], local: Path) -> None:
    """Reject a completed remote asset whose immutable evidence conflicts locally."""

    if asset.get("state") != "uploaded":
        raise PublishError(
            f"release asset {asset.get('name')!r} is in unexpected state "
            f"{asset.get('state')!r}"
        )
    size = asset.get("size")
    local_size = local.stat().st_size
    if size != local_size:
        raise PublishError(
            f"release asset {local.name!r} has remote size {size!r}, "
            f"local size {local_size}"
        )
    digest = asset.get("digest")
    if digest is not None:
        expected_digest = f"sha256:{release_assets.sha256_file(local)}"
        if digest != expected_digest:
            raise PublishError(
                f"release asset {local.name!r} has digest {digest!r}; "
                f"expected {expected_digest!r}"
            )


def verify_remote_downloads(
    gateway: Any,
    repository: str,
    remote_assets: dict[str, dict[str, Any]],
    *,
    version: str,
    tag: str,
    commit: str,
) -> None:
    """Download every remote asset by ID and re-run complete-set verification."""

    expected_names = set(release_assets.expected_archive_names(tag)) | {
        release_assets.CHECKSUM_FILE
    }
    if set(remote_assets) != expected_names:
        raise PublishError(
            f"remote asset set is {sorted(remote_assets)!r}; "
            f"expected {sorted(expected_names)!r}"
        )
    with tempfile.TemporaryDirectory(prefix="release-download-") as temporary:
        directory = Path(temporary)
        for name in sorted(remote_assets):
            asset_id = remote_assets[name].get("id")
            if not isinstance(asset_id, int):
                raise PublishError(f"release asset {name!r} has no integer ID")
            gateway.download_asset(repository, asset_id, directory / name)
        release_assets.verify_release_set(
            directory, version=version, tag=tag, commit=commit
        )


def publish_release(
    gateway: Any,
    repository: str,
    assets_directory: Path,
    *,
    version: str,
    tag: str,
    commit: str,
    run_url: str,
) -> str:
    """Create or resume a workflow draft, verify remote bytes, and publish once."""

    local_assets = expected_local_assets(
        assets_directory, version=version, tag=tag, commit=commit
    )
    remote_commit = gateway.resolve_tag_commit(repository, tag)
    if remote_commit != commit:
        raise PublishError(
            f"remote tag {tag} resolves to {remote_commit!r}, not validated commit {commit}"
        )

    candidate = find_release(gateway.list_releases(repository), tag)
    if candidate is None:
        candidate = gateway.create_draft(
            repository,
            tag=tag,
            commit=commit,
            body=release_body(tag, commit, run_url),
        )
    require_release_identity(candidate, tag=tag, commit=commit)

    release_id = candidate.get("id")
    if not isinstance(release_id, int):
        raise PublishError("release has no integer ID")
    candidate = gateway.get_release(repository, release_id)
    require_release_identity(candidate, tag=tag, commit=commit)
    remote_assets = asset_map(candidate)
    unexpected = set(remote_assets).difference(local_assets)
    if unexpected:
        raise PublishError(
            f"release contains conflicting unexpected assets: {', '.join(sorted(unexpected))}"
        )

    # An `open` asset is an upload GitHub never finished. Only a draft this workflow owns
    # may have one removed, and only so the same bytes can be sent again.
    incomplete = [
        asset for asset in remote_assets.values() if asset.get("state") == "open"
    ]
    for asset in incomplete:
        if candidate.get("draft") is not True:
            raise PublishError(
                f"published release contains incomplete asset {asset.get('name')!r}"
            )
        asset_id = asset.get("id")
        if not isinstance(asset_id, int):
            raise PublishError("incomplete release asset has no integer ID")
        gateway.delete_asset(repository, asset_id)

    if incomplete:
        candidate = gateway.get_release(repository, release_id)
        require_release_identity(candidate, tag=tag, commit=commit)
        remote_assets = asset_map(candidate)
        unexpected = set(remote_assets).difference(local_assets)
        if unexpected:
            raise PublishError(
                "release contains conflicting unexpected assets: "
                f"{', '.join(sorted(unexpected))}"
            )

    for name, asset in remote_assets.items():
        require_uploaded_asset_matches(asset, local_assets[name])

    if candidate.get("draft") is not True:
        if set(remote_assets) != set(local_assets):
            raise PublishError(
                "published release is missing one or more required assets"
            )
        verify_remote_downloads(
            gateway, repository, remote_assets, version=version, tag=tag, commit=commit
        )
        if gateway.resolve_tag_commit(repository, tag) != commit:
            raise PublishError("tag changed while verifying an already-published release")
        return "already-published"

    for name in sorted(set(local_assets).difference(remote_assets)):
        try:
            gateway.upload_asset(repository, tag, local_assets[name])
        except Exception as error:
            raise PublishError(
                f"upload of {name!r} failed; workflow-owned draft {release_id} was "
                f"retained for a same-tag retry: {error}"
            ) from error

    candidate = gateway.get_release(repository, release_id)
    require_release_identity(candidate, tag=tag, commit=commit)
    remote_assets = asset_map(candidate)
    if set(remote_assets) != set(local_assets):
        raise PublishError(
            f"draft asset set is {sorted(remote_assets)!r}; "
            f"expected {sorted(local_assets)!r}"
        )
    for name, asset in remote_assets.items():
        require_uploaded_asset_matches(asset, local_assets[name])
    verify_remote_downloads(
        gateway, repository, remote_assets, version=version, tag=tag, commit=commit
    )

    if gateway.resolve_tag_commit(repository, tag) != commit:
        raise PublishError("tag changed after asset verification; draft retained")
    published = gateway.publish(repository, release_id)
    require_release_identity(published, tag=tag, commit=commit)
    if published.get("draft") is not False:
        raise PublishError(
            "GitHub did not transition the verified draft to published state"
        )
    if gateway.resolve_tag_commit(repository, tag) != commit:
        raise PublishError("tag changed while publishing the release")
    final_assets = asset_map(gateway.get_release(repository, release_id))
    if set(final_assets) != set(local_assets):
        raise PublishError("published release asset set changed after publication")
    return "published"


class GhGateway:
    """Small shell-free adapter around the authenticated GitHub CLI."""

    def __init__(self, working_directory: Path) -> None:
        """Bind GitHub CLI calls to a repository checkout."""

        self.working_directory = working_directory.resolve()
        if not os.environ.get("GH_TOKEN"):
            raise PublishError("GH_TOKEN is required for release publication")

    def _run(
        self,
        arguments: Sequence[str],
        *,
        input_bytes: bytes | None = None,
        output_file: Any | None = None,
    ) -> bytes:
        completed = subprocess.run(
            arguments,
            cwd=self.working_directory,
            input=input_bytes,
            stdout=output_file if output_file is not None else subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if completed.returncode != 0:
            stderr = completed.stderr.decode(errors="replace").strip()
            command = " ".join(arguments[:4])
            raise PublishError(
                f"{command} failed with status {completed.returncode}: {stderr}"
            )
        return b"" if output_file is not None else completed.stdout

    def _api(
        self,
        endpoint: str,
        *,
        method: str = "GET",
        payload: dict[str, Any] | None = None,
        paginate: bool = False,
    ) -> Any:
        arguments = [
            "gh",
            "api",
            endpoint,
            "--method",
            method,
            "--header",
            f"X-GitHub-Api-Version: {API_VERSION}",
        ]
        if paginate:
            arguments.extend(("--paginate", "--slurp"))
        input_bytes = None
        if payload is not None:
            arguments.extend(("--input", "-"))
            input_bytes = json.dumps(payload, separators=(",", ":")).encode()
        output = self._run(arguments, input_bytes=input_bytes)
        if not output:
            return None
        try:
            return json.loads(output)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise PublishError(
                f"GitHub API returned invalid JSON for {endpoint}"
            ) from error

    def resolve_tag_commit(self, repository: str, tag: str) -> str:
        """Peel lightweight or annotated tag objects to one commit."""

        encoded_tag = quote(tag, safe="")
        reference = self._api(f"repos/{repository}/git/ref/tags/{encoded_tag}")
        if not isinstance(reference, dict) or not isinstance(
            reference.get("object"), dict
        ):
            raise PublishError(f"GitHub did not return tag object metadata for {tag}")
        target = reference["object"]
        for _ in range(8):
            object_type = target.get("type")
            object_sha = target.get("sha")
            if not isinstance(object_sha, str):
                raise PublishError(f"tag {tag} object has no SHA")
            if object_type == "commit":
                return release_assets.validate_commit(object_sha)
            if object_type != "tag":
                raise PublishError(
                    f"tag {tag} points to unsupported object type {object_type!r}"
                )
            annotated = self._api(f"repos/{repository}/git/tags/{object_sha}")
            if not isinstance(annotated, dict) or not isinstance(
                annotated.get("object"), dict
            ):
                raise PublishError(f"annotated tag object {object_sha} is malformed")
            target = annotated["object"]
        raise PublishError(f"tag {tag} exceeds the bounded annotated-tag chain")

    def list_releases(self, repository: str) -> list[dict[str, Any]]:
        """List published and workflow-visible draft releases across every page."""

        pages = self._api(f"repos/{repository}/releases?per_page=100", paginate=True)
        return flatten_release_pages(pages)

    def create_draft(
        self, repository: str, *, tag: str, commit: str, body: str
    ) -> dict[str, Any]:
        """Create one generated-notes draft for an already-existing tag."""

        value = self._api(
            f"repos/{repository}/releases",
            method="POST",
            payload={
                "tag_name": tag,
                "target_commitish": commit,
                "name": tag,
                "body": body,
                "draft": True,
                "prerelease": False,
                "generate_release_notes": True,
            },
        )
        if not isinstance(value, dict):
            raise PublishError("GitHub did not return the created draft release")
        return value

    def get_release(self, repository: str, release_id: int) -> dict[str, Any]:
        """Reload one release and its current asset state."""

        value = self._api(f"repos/{repository}/releases/{release_id}")
        if not isinstance(value, dict):
            raise PublishError(f"GitHub did not return release {release_id}")
        return value

    def delete_asset(self, repository: str, asset_id: int) -> None:
        """Delete only an ownership-checked incomplete draft asset."""

        self._api(f"repos/{repository}/releases/assets/{asset_id}", method="DELETE")

    def upload_asset(self, repository: str, tag: str, path: Path) -> None:
        """Upload one previously absent asset without clobbering."""

        self._run(
            ("gh", "release", "upload", tag, str(path), "--repo", repository)
        )

    def download_asset(self, repository: str, asset_id: int, path: Path) -> None:
        """Stream one authenticated release asset to an isolated file."""

        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("xb") as output:
            self._run(
                (
                    "gh",
                    "api",
                    f"repos/{repository}/releases/assets/{asset_id}",
                    "--header",
                    "Accept: application/octet-stream",
                    "--header",
                    f"X-GitHub-Api-Version: {API_VERSION}",
                ),
                output_file=output,
            )

    def publish(self, repository: str, release_id: int) -> dict[str, Any]:
        """Publish one fully verified draft without changing its tag fields."""

        value = self._api(
            f"repos/{repository}/releases/{release_id}",
            method="PATCH",
            payload={"draft": False},
        )
        if not isinstance(value, dict):
            raise PublishError(f"GitHub did not return published release {release_id}")
        return value


def argument_parser() -> argparse.ArgumentParser:
    """Build the controlled-publisher command-line parser."""

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository-path", type=Path, default=Path.cwd())
    parser.add_argument("--repo", required=True)
    parser.add_argument("--assets-directory", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--run-url", required=True)
    return parser


def run(arguments: Sequence[str]) -> int:
    """Validate local state, perform controlled publication, and report outcome."""

    options = argument_parser().parse_args(arguments)
    outcome = publish_release(
        GhGateway(options.repository_path),
        options.repo,
        options.assets_directory,
        version=options.version,
        tag=options.tag,
        commit=options.commit,
        run_url=options.run_url,
    )
    print(f"Release {options.tag}: {outcome}")
    return 0


def main(arguments: Sequence[str] | None = None) -> int:
    """Convert publication conflicts into a stable nonzero status."""

    try:
        return run(sys.argv[1:] if arguments is None else arguments)
    except (
        OSError,
        PublishError,
        release_assets.ReleaseError,
        UnicodeError,
        json.JSONDecodeError,
    ) as error:
        print(f"release publication failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
