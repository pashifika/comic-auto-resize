#!/usr/bin/env python3
"""Self-tests for the shell-completion acceptance harness.

A harness that stopped asserting would pass every run and prove nothing, which is a worse
failure than a red job because nobody looks at it. These are the assertions that would go
quiet: the case list, the verdict, and the isolation the fixtures promise.

Runs in `hygiene`, on a host with none of the four shells installed, so nothing here may
start one.
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest import mock

from shell_completion_acceptance import (
    CASE_ORDER,
    PRODUCT,
    SHELL_ORDER,
    AcceptanceError,
    CompletionCase,
    Fixture,
    completion_cases,
    install_completion,
    main,
    menu_candidates,
    require_interpreter,
    verify_case,
)


class HarnessTests(unittest.TestCase):
    def test_the_case_list_is_the_one_the_harness_advertises(self) -> None:
        """A dropped case would otherwise shrink the run and still report success."""
        self.assertEqual(tuple(case.name for case in completion_cases()), CASE_ORDER)
        for case in completion_cases():
            self.assertTrue(case.expected, f"case {case.name!r} asserts nothing")
            self.assertTrue(case.line.startswith(PRODUCT), f"case {case.name!r} is off-product")

    def test_a_missing_candidate_fails(self) -> None:
        case = CompletionCase(name="dct", line=f"{PRODUCT} --dct ", expected=("float", "ifast"))
        with self.assertRaisesRegex(AcceptanceError, "missing=\\['ifast'\\]"):
            verify_case("fish", case, "float\n")

    def test_a_forbidden_candidate_fails_even_when_the_expected_ones_are_there(self) -> None:
        """The stock PowerShell regression: a value position offering the option list back."""
        case = CompletionCase(
            name="dct",
            line=f"{PRODUCT} --dct ",
            expected=("float", "ifast", "islow"),
            forbidden=("--dct",),
        )
        with self.assertRaisesRegex(AcceptanceError, "forbidden=\\['--dct'\\]"):
            verify_case("powershell", case, "float\nifast\nislow\n--dct\n")

    def test_a_shell_diagnostic_fails_a_case_that_otherwise_matched(self) -> None:
        case = CompletionCase(name="dct", line=f"{PRODUCT} --dct ", expected=("float",))
        with self.assertRaisesRegex(AcceptanceError, "diagnostics="):
            verify_case("fish", case, "float\ncompopt: not currently executing\n")

    def test_a_terminal_that_printed_no_menu_yields_no_candidates(self) -> None:
        """Silence must fail a case, not satisfy it.

        A terminal shell that offered nothing echoes the command line and stops, which is
        one block. Reading candidates out of that block would let every case pass on a
        script the shell never sourced.
        """
        self.assertEqual(menu_candidates(f"CAR_PROMPT> {PRODUCT} --r\n"), set())
        case = CompletionCase(name="prefix", line=f"{PRODUCT} --r", expected=("--ratio",))
        with self.assertRaisesRegex(AcceptanceError, "missing="):
            verify_case("bash", case, f"CAR_PROMPT> {PRODUCT} --r\n")

    def test_an_advertised_shell_that_is_absent_fails_closed(self) -> None:
        """A shell the tool ships a script for is never reported as unavailable."""
        with mock.patch("shell_completion_acceptance.shutil.which", return_value=None):
            with self.assertRaisesRegex(AcceptanceError, "required-interpreter"):
                require_interpreter("fish")

    def test_every_installed_file_stays_inside_the_isolated_home(self) -> None:
        for shell in SHELL_ORDER:
            with self.subTest(shell=shell), Fixture(shell) as fixture:
                installation = install_completion(
                    shell, b"# script\n", fixture, f"/usr/bin/{shell}", Path("/usr/bin")
                )
                self.assertTrue(
                    installation.script.is_relative_to(fixture.home),
                    f"{shell}: installed outside the fixture home at {installation.script}",
                )

    def test_fixture_cleanup_removes_only_its_own_tree(self) -> None:
        with tempfile.TemporaryDirectory() as parent:
            sibling = Path(parent) / "keep-me"
            sibling.write_text("keep\n", encoding="utf-8")
            with Fixture("bash", parent=Path(parent)) as fixture:
                root = fixture.root
            self.assertFalse(root.exists())
            self.assertEqual(sibling.read_text(encoding="utf-8"), "keep\n")

    def test_a_shell_named_twice_is_refused_before_any_binary_runs(self) -> None:
        """Two runs of one shell must not read like coverage of two."""
        with self.assertRaisesRegex(AcceptanceError, "exactly once"):
            main(["--binary", "/nonexistent/comic-auto-resize", "--shell", "bash", "--shell", "bash"])


if __name__ == "__main__":
    unittest.main()
