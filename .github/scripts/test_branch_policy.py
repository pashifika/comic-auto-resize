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
    # A development line reaches main.
    ("main", "dev/2.0.x", REPOSITORY, REPOSITORY, AUTHOR),
    ("main", "dev/10.11.x", REPOSITORY, REPOSITORY, AUTHOR),
    # Every supported topic prefix reaches a development line.
    ("dev/2.0.x", "feat/split-spreads", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/2.0.x", "fix/bmp-header-match", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/2.0.x", "perf/scaled-decode", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/2.0.x", "refactor/source-enum", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/2.0.x", "docs/readme", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/2.0.x", "test/fixtures", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/2.0.x", "build/nasm", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/2.0.x", "ci/gate", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/2.0.x", "chore/deps", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/2.0.x", "revert/bad-merge", REPOSITORY, REPOSITORY, AUTHOR),
    # Dependabot is exempt on both bases, from the base repository only.
    ("main", "dependabot/cargo/clap-4.6.7", REPOSITORY, REPOSITORY, DEPENDABOT),
    ("dev/2.0.x", "dependabot/cargo/clap-4.6.7", REPOSITORY, REPOSITORY, DEPENDABOT),
]

REJECTED: list[tuple[str, str, str, str, str]] = [
    # A topic branch may not skip the development line.
    ("main", "feat/split-spreads", REPOSITORY, REPOSITORY, AUTHOR),
    ("main", "fix/urgent", REPOSITORY, REPOSITORY, AUTHOR),
    # A fork may not target main even with a well-formed head.
    ("main", "dev/2.0.x", REPOSITORY, FORK, AUTHOR),
    # An unrecognised prefix is not a topic branch.
    ("dev/2.0.x", "wip/experiment", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/2.0.x", "feature/split", REPOSITORY, REPOSITORY, AUTHOR),
    # A prefix with no slug is not a topic branch.
    ("dev/2.0.x", "feat/", REPOSITORY, REPOSITORY, AUTHOR),
    ("dev/2.0.x", "feat", REPOSITORY, REPOSITORY, AUTHOR),
    # A development line is not a topic branch for another development line.
    ("dev/2.0.x", "dev/2.1.x", REPOSITORY, REPOSITORY, AUTHOR),
    # Malformed development lines are not development lines.
    ("main", "dev/2.x", REPOSITORY, REPOSITORY, AUTHOR),
    ("main", "dev/2.0.0", REPOSITORY, REPOSITORY, AUTHOR),
    ("main", "dev/2.0.x-hotfix", REPOSITORY, REPOSITORY, AUTHOR),
    # `master` is frozen; nothing targets it, and it is not a valid base.
    ("master", "dev/2.0.x", REPOSITORY, REPOSITORY, AUTHOR),
    ("release", "dev/2.0.x", REPOSITORY, REPOSITORY, AUTHOR),
    # The Dependabot exemption is bound to the actor, the branch shape, and the
    # repository together; loosening any one of the three must not admit the request.
    ("main", "dependabot/cargo/clap-4.6.7", REPOSITORY, REPOSITORY, AUTHOR),
    ("main", "feat/looks-like-a-bot", REPOSITORY, REPOSITORY, DEPENDABOT),
    ("main", "dependabot/cargo/clap-4.6.7", REPOSITORY, FORK, DEPENDABOT),
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
