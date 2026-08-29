#!/usr/bin/env python3
"""Validate the pull-request branch flow used by comic-auto-resize.

Topic branches merge into a development line; a development line merges into `main`.
A topic branch may not reach `main` without passing through one.
"""

from __future__ import annotations

import re
import sys

DEVELOPMENT_BRANCH = re.compile(r"dev/[0-9]+\.[0-9]+\.x")
DEPENDABOT_BRANCH = re.compile(r"dependabot/.+")
TOPIC_BRANCH = re.compile(
    r"(?:feat|fix|perf|refactor|docs|test|build|ci|chore|revert)/.+"
)


def is_dependabot_update(
    head: str, base_repository: str, head_repository: str, author_login: str
) -> bool:
    """Return whether the pull request is a same-repository Dependabot update."""

    return (
        author_login == "dependabot[bot]"
        and head_repository == base_repository
        and DEPENDABOT_BRANCH.fullmatch(head) is not None
    )


def validate_branch_flow(
    base: str,
    head: str,
    base_repository: str,
    head_repository: str,
    author_login: str,
) -> str | None:
    """Return an error message when *head* is not allowed to target *base*."""

    if base == "main":
        if is_dependabot_update(head, base_repository, head_repository, author_login):
            return None
        if head_repository != base_repository:
            return "pull requests into main must come from the base repository"
        if DEVELOPMENT_BRANCH.fullmatch(head):
            return None
        return "pull requests into main must come from dev/<major>.<minor>.x"

    if DEVELOPMENT_BRANCH.fullmatch(base):
        if is_dependabot_update(head, base_repository, head_repository, author_login):
            return None
        if TOPIC_BRANCH.fullmatch(head):
            return None
        return (
            "pull requests into a development line must come from a supported "
            "topic branch with a non-empty slug"
        )

    return "the pull-request base must be main or dev/<major>.<minor>.x"


def main(arguments: list[str]) -> int:
    """Run the command-line branch-policy check."""

    if len(arguments) != 6:
        print(
            "usage: branch_policy.py <base> <head> <base-repository> "
            "<head-repository> <author-login>",
            file=sys.stderr,
        )
        return 2

    base, head, base_repository, head_repository, author_login = arguments[1:]
    error = validate_branch_flow(
        base, head, base_repository, head_repository, author_login
    )
    if error is not None:
        print(f"branch policy rejected {head} -> {base}: {error}", file=sys.stderr)
        return 1

    print(f"branch policy accepted {head} -> {base}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
