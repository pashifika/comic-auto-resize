#!/usr/bin/env python3
"""Verify the branch-policy rules before they are trusted to judge a pull request.

The check itself is folded into the `hygiene` job rather than owning a status context of
its own, so a silent bug here would fail the whole gate with a misleading cause. These
cases exist so that cannot happen unnoticed.
"""

from __future__ import annotations

import sys

from branch_policy import validate_branch_flow

REPOSITORY = "pashifika/comic-auto-resize"
FORK = "someone-else/comic-auto-resize"
AUTHOR = "pashifika"
DEPENDABOT = "dependabot[bot]"

ACCEPTED: list[tuple[str, str, str, str, str]] = [
    # A development line reaches main, whatever its topic is called.
    ("main", "dev/spread-split", REPOSITORY, REPOSITORY, AUTHOR),
    ("main", "dev/2.0.x", REPOSITORY, REPOSITORY, AUTHOR),
    # Every supported topic prefix reaches a development line.
    ("dev/spread-split", "feat/split-spreads", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/spread-split", "fix/bmp-header-match", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/spread-split", "perf/scaled-decode", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/spread-split", "refactor/source-enum", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/spread-split", "docs/readme", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/spread-split", "test/fixtures", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/spread-split", "build/nasm", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/spread-split", "ci/gate", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/spread-split", "chore/release-2-2-0", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/spread-split", "revert/bad-merge", REPOSITORY, REPOSITORY, AUTHOR),
    # Promotion synchronization: a promotion leaves the development line one merge commit
    # behind, and the development ruleset refuses a direct fast-forward push.
    ("dev/spread-split", "main", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/2.0.x", "main", REPOSITORY, REPOSITORY, AUTHOR),
    # Dependabot is exempt on both bases, from the base repository only.
    ("main", "dependabot/cargo/clap-4.6.7", REPOSITORY, REPOSITORY, DEPENDABOT),
    ("dev/spread-split", "dependabot/cargo/clap-4.6.7", REPOSITORY, REPOSITORY, DEPENDABOT),
]

REJECTED: list[tuple[str, str, str, str, str]] = [
    # A topic branch may not skip the development line.
    ("main", "feat/split-spreads", REPOSITORY, REPOSITORY, AUTHOR),
    ("main", "fix/urgent", REPOSITORY, REPOSITORY, AUTHOR),
    ("main", "chore/release-2-2-0", REPOSITORY, REPOSITORY, AUTHOR),
    # A fork may not target main even with a well-formed head.
    ("main", "dev/spread-split", REPOSITORY, FORK, AUTHOR),
    # An unrecognised prefix is not a topic branch.
    ("dev/spread-split", "wip/experiment", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/spread-split", "feature/split", REPOSITORY, REPOSITORY, AUTHOR),
    # A prefix with no slug is not a topic branch.
    ("dev/spread-split", "feat/", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/spread-split", "feat", REPOSITORY, REPOSITORY, AUTHOR),
    # A development line is not a topic branch for another development line.
    ("dev/spread-split", "dev/another-topic", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/2.0.x", "dev/spread-split", REPOSITORY, REPOSITORY, AUTHOR),
    # A development name needs exactly one non-empty topic component, on either side.
    ("main", "dev/", REPOSITORY, REPOSITORY, AUTHOR),
    ("main", "dev", REPOSITORY, REPOSITORY, AUTHOR),
    ("main", "dev/spread/split", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/", "feat/split-spreads", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/spread/split", "feat/split-spreads", REPOSITORY, REPOSITORY, AUTHOR),
    # `master` is frozen; nothing targets it, and it is not a valid base.
    ("master", "dev/spread-split", REPOSITORY, REPOSITORY, AUTHOR),
    ("release", "dev/spread-split", REPOSITORY, REPOSITORY, AUTHOR),
    # The Dependabot exemption is bound to the actor, the branch shape, and the
    # repository together; loosening any one of the three must not admit the request.
    ("main", "dependabot/cargo/clap-4.6.7", REPOSITORY, REPOSITORY, AUTHOR),
    ("main", "feat/looks-like-a-bot", REPOSITORY, REPOSITORY, DEPENDABOT),
    ("main", "dependabot/cargo/clap-4.6.7", REPOSITORY, FORK, DEPENDABOT),
    # Synchronization is bound to the base repository: a fork's `main` is unrelated
    # content wearing a trusted name.
    ("dev/spread-split", "main", REPOSITORY, FORK, AUTHOR),
    # Only `main` synchronizes. `master` is the Go reference branch, and its tree is not
    # what a development line catches up to.
    ("dev/spread-split", "master", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/spread-split", "main-sync", REPOSITORY, REPOSITORY, AUTHOR),
]


def main() -> int:
    """Run every case and report the first divergence per direction."""

    failures: list[str] = []

    for base, head, base_repo, head_repo, author in ACCEPTED:
        error = validate_branch_flow(base, head, base_repo, head_repo, author)
        if error is not None:
            failures.append(f"expected {head} -> {base} to be accepted, got: {error}")

    for base, head, base_repo, head_repo, author in REJECTED:
        error = validate_branch_flow(base, head, base_repo, head_repo, author)
        if error is None:
            failures.append(f"expected {head} -> {base} to be rejected, it was accepted")

    if failures:
        for failure in failures:
            print(f"FAIL {failure}", file=sys.stderr)
        print(f"{len(failures)} branch-policy case(s) failed", file=sys.stderr)
        return 1

    total = len(ACCEPTED) + len(REJECTED)
    print(f"branch policy: {total} case(s) checked, all as expected")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
