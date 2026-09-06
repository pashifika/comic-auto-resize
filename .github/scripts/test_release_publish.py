#!/usr/bin/env python3
"""Self-tests for the controlled release publisher.

Nothing here proves that GitHub publishes a release; only a real run does that. What these
cases pin down is the half that is dangerous to get wrong and impossible to test in
production: which remote states this publisher agrees to change, and which ones it refuses
to touch. Every refusal below is a state where the alternative is overwriting bytes
somebody may already have downloaded.
"""

from __future__ import annotations

import hashlib
import shutil
import tempfile
import unittest
from pathlib import Path
from typing import Any

import release as release_assets
from release_publish import PublishError, publish_release, release_marker
from test_release import COMMIT, OTHER_COMMIT, TAG, VERSION, macho, portable_executable

RUN_URL = "https://github.com/pashifika/comic-auto-resize/actions/runs/1"


class FakeGateway:
    """An in-memory stand-in for the GitHub Releases endpoints the publisher uses."""

    def __init__(self, *, tag_commits: list[str] | None = None) -> None:
        self.tag_commits = tag_commits or [COMMIT]
        self.releases: dict[int, dict[str, Any]] = {}
        self.blobs: dict[int, bytes] = {}
        self.next_id = 100
        self.calls: list[str] = []
        self.resolutions = 0

    def _identifier(self) -> int:
        self.next_id += 1
        return self.next_id

    def resolve_tag_commit(self, repository: str, tag: str) -> str:
        """Answer successive resolutions, holding the last value once exhausted."""

        self.calls.append("resolve_tag_commit")
        answer = self.tag_commits[min(self.resolutions, len(self.tag_commits) - 1)]
        self.resolutions += 1
        return answer

    def list_releases(self, repository: str) -> list[dict[str, Any]]:
        self.calls.append("list_releases")
        return list(self.releases.values())

    def create_draft(
        self, repository: str, *, tag: str, commit: str, body: str
    ) -> dict[str, Any]:
        self.calls.append("create_draft")
        release = {
            "id": self._identifier(),
            "tag_name": tag,
            "name": tag,
            "body": body,
            "draft": True,
            "prerelease": False,
            "assets": [],
        }
        self.releases[release["id"]] = release
        return release

    def get_release(self, repository: str, release_id: int) -> dict[str, Any]:
        self.calls.append("get_release")
        return self.releases[release_id]

    def delete_asset(self, repository: str, asset_id: int) -> None:
        self.calls.append("delete_asset")
        for release in self.releases.values():
            release["assets"] = [
                asset for asset in release["assets"] if asset["id"] != asset_id
            ]

    def upload_asset(self, repository: str, tag: str, path: Path) -> None:
        self.calls.append("upload_asset")
        release = next(
            release for release in self.releases.values() if release["tag_name"] == tag
        )
        data = path.read_bytes()
        asset_id = self._identifier()
        self.blobs[asset_id] = data
        release["assets"].append(
            {
                "id": asset_id,
                "name": path.name,
                "state": "uploaded",
                "size": len(data),
                "digest": f"sha256:{hashlib.sha256(data).hexdigest()}",
            }
        )

    def download_asset(self, repository: str, asset_id: int, path: Path) -> None:
        self.calls.append("download_asset")
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(self.blobs[asset_id])

    def publish(self, repository: str, release_id: int) -> dict[str, Any]:
        self.calls.append("publish")
        self.releases[release_id]["draft"] = False
        return self.releases[release_id]

    # Test helpers.

    def seed(self, *, draft: bool, marker_commit: str = COMMIT) -> dict[str, Any]:
        release = {
            "id": self._identifier(),
            "tag_name": TAG,
            "name": TAG,
            "body": f"{release_marker(TAG, marker_commit)}\n\nowned",
            "draft": draft,
            "prerelease": False,
            "assets": [],
        }
        self.releases[release["id"]] = release
        return release

    def attach(self, release: dict[str, Any], path: Path, *, state: str = "uploaded") -> None:
        data = path.read_bytes()
        asset_id = self._identifier()
        self.blobs[asset_id] = data
        release["assets"].append(
            {
                "id": asset_id,
                "name": path.name,
                "state": state,
                "size": len(data) if state == "uploaded" else 0,
                "digest": f"sha256:{hashlib.sha256(data).hexdigest()}",
            }
        )


class PublisherTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="release-publish-"))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        repository = self.root / "repository"
        repository.mkdir()
        (repository / release_assets.LICENSE_FILE).write_text("licence\n", encoding="utf-8")
        (repository / release_assets.NOTICE_FILE).write_text("notice\n", encoding="utf-8")
        third_party = self.root / release_assets.THIRD_PARTY_FILE
        third_party.write_text("third-party\n", encoding="utf-8")

        staged = self.root / "staged"
        for target in release_assets.TARGETS:
            binaries = self.root / f"bin-{target.name}"
            binaries.mkdir()
            path = binaries / release_assets.executable_name(target)
            path.write_bytes(
                portable_executable("KERNEL32.dll")
                if target.extension == ".zip"
                else macho("/usr/lib/libSystem.B.dylib")
            )
            path.chmod(0o755)
            release_assets.package_release(
                repository,
                binaries,
                third_party,
                staged,
                target=target,
                version=VERSION,
                tag=TAG,
                commit=COMMIT,
            )
        self.assets = self.root / "verified"
        release_assets.aggregate_release(
            staged, self.assets, version=VERSION, tag=TAG, commit=COMMIT
        )
        self.names = sorted(
            [*release_assets.expected_archive_names(TAG), release_assets.CHECKSUM_FILE]
        )

    def publish(self, gateway: FakeGateway) -> str:
        return publish_release(
            gateway,
            "pashifika/comic-auto-resize",
            self.assets,
            version=VERSION,
            tag=TAG,
            commit=COMMIT,
            run_url=RUN_URL,
        )

    def test_a_first_publication_uploads_verifies_and_publishes_once(self) -> None:
        gateway = FakeGateway()
        self.assertEqual(self.publish(gateway), "published")
        release = next(iter(gateway.releases.values()))
        self.assertFalse(release["draft"])
        self.assertEqual(sorted(asset["name"] for asset in release["assets"]), self.names)
        self.assertEqual(gateway.calls.count("publish"), 1)
        # Every uploaded byte is read back from the remote before publication.
        self.assertEqual(gateway.calls.count("download_asset"), len(self.names))

    def test_an_interrupted_draft_resumes_only_the_missing_asset(self) -> None:
        gateway = FakeGateway()
        draft = gateway.seed(draft=True)
        gateway.attach(draft, self.assets / self.names[0])
        self.assertEqual(self.publish(gateway), "published")
        self.assertEqual(gateway.calls.count("upload_asset"), len(self.names) - 1)
        self.assertEqual(
            sorted(asset["name"] for asset in draft["assets"]), self.names
        )

    def test_an_incomplete_asset_is_replaced_rather_than_left_behind(self) -> None:
        """An `open` asset is an upload GitHub never finished; the bytes are resent."""
        gateway = FakeGateway()
        draft = gateway.seed(draft=True)
        gateway.attach(draft, self.assets / self.names[0], state="open")
        self.assertEqual(self.publish(gateway), "published")
        self.assertEqual(gateway.calls.count("delete_asset"), 1)
        self.assertEqual(gateway.calls.count("upload_asset"), len(self.names))

    def test_a_published_release_is_verified_without_mutation(self) -> None:
        gateway = FakeGateway()
        published = gateway.seed(draft=False)
        for name in self.names:
            gateway.attach(published, self.assets / name)
        self.assertEqual(self.publish(gateway), "already-published")
        self.assertNotIn("upload_asset", gateway.calls)
        self.assertNotIn("delete_asset", gateway.calls)
        self.assertNotIn("publish", gateway.calls)

    def test_a_published_release_with_conflicting_bytes_stops(self) -> None:
        """Replacing a downloaded asset is the one outcome this must never produce."""
        gateway = FakeGateway()
        published = gateway.seed(draft=False)
        for name in self.names:
            gateway.attach(published, self.assets / name)
        published["assets"][0]["size"] += 1
        with self.assertRaises(PublishError):
            self.publish(gateway)
        self.assertNotIn("upload_asset", gateway.calls)
        self.assertNotIn("publish", gateway.calls)

    def test_a_published_release_missing_an_asset_stops(self) -> None:
        gateway = FakeGateway()
        published = gateway.seed(draft=False)
        for name in self.names[:-1]:
            gateway.attach(published, self.assets / name)
        with self.assertRaises(PublishError):
            self.publish(gateway)
        self.assertNotIn("upload_asset", gateway.calls)

    def test_a_published_release_with_an_incomplete_asset_stops(self) -> None:
        gateway = FakeGateway()
        published = gateway.seed(draft=False)
        for name in self.names:
            gateway.attach(published, self.assets / name)
        published["assets"][0]["state"] = "open"
        with self.assertRaises(PublishError):
            self.publish(gateway)
        self.assertNotIn("delete_asset", gateway.calls)

    def test_a_release_this_workflow_does_not_own_stops(self) -> None:
        """A hand-made draft for the same tag is somebody else's intent."""
        gateway = FakeGateway()
        foreign = gateway.seed(draft=True)
        foreign["body"] = "prepared by hand"
        with self.assertRaises(PublishError):
            self.publish(gateway)
        self.assertNotIn("upload_asset", gateway.calls)

    def test_a_draft_bound_to_another_commit_stops(self) -> None:
        gateway = FakeGateway()
        gateway.seed(draft=True, marker_commit=OTHER_COMMIT)
        with self.assertRaises(PublishError):
            self.publish(gateway)

    def test_an_unexpected_asset_stops(self) -> None:
        gateway = FakeGateway()
        draft = gateway.seed(draft=True)
        stray = self.root / "notes.txt"
        stray.write_text("stray\n", encoding="utf-8")
        gateway.attach(draft, stray)
        with self.assertRaises(PublishError):
            self.publish(gateway)
        self.assertNotIn("publish", gateway.calls)

    def test_a_tag_pointing_elsewhere_stops_before_any_write(self) -> None:
        gateway = FakeGateway(tag_commits=[OTHER_COMMIT])
        with self.assertRaises(PublishError):
            self.publish(gateway)
        self.assertEqual(gateway.releases, {})

    def test_a_tag_moved_after_verification_stops_before_publication(self) -> None:
        """Verified bytes belong to the commit they were built from, not to the ref."""
        gateway = FakeGateway(tag_commits=[COMMIT, OTHER_COMMIT])
        with self.assertRaises(PublishError):
            self.publish(gateway)
        self.assertNotIn("publish", gateway.calls)
        self.assertTrue(all(release["draft"] for release in gateway.releases.values()))

    def test_a_local_asset_set_that_does_not_verify_stops(self) -> None:
        """The publisher revalidates rather than trusting the aggregate job's word."""
        (self.assets / release_assets.CHECKSUM_FILE).write_text(
            "0" * 64 + "  " + self.names[0] + "\n", encoding="ascii"
        )
        gateway = FakeGateway()
        with self.assertRaises(release_assets.ReleaseError):
            self.publish(gateway)
        self.assertEqual(gateway.calls, [])

    def test_a_marker_is_bound_to_both_tag_and_commit(self) -> None:
        self.assertIn(TAG, release_marker(TAG, COMMIT))
        self.assertIn(COMMIT, release_marker(TAG, COMMIT))
        self.assertNotEqual(
            release_marker(TAG, COMMIT), release_marker(TAG, OTHER_COMMIT)
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
