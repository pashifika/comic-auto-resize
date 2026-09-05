#!/usr/bin/env python3
"""Self-tests for the shell-completion acceptance harness.

A harness that stopped asserting would pass every run and prove nothing, which is a worse
failure than a red job because nobody looks at it. These are the assertions that would go
quiet: the case list and its data, the verdict, the ran-everything check, and the isolation
the fixtures promise.

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
    assert_ran,
    completion_cases,
    install_completion,
    isolated_environment,
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
            # A case asserts through candidates, through the resulting buffer, or through the
            # set it forbids. `enum-no-match` and `after-terminator` expect nothing on
            # purpose: what they prove is negative, and the companion test below makes sure no
            # case is left with nothing to fail on.
            self.assertTrue(
                case.expected or case.completed or case.forbidden,
                f"case {case.name!r} asserts nothing at all",
            )
            self.assertTrue(case.line.startswith(PRODUCT), f"case {case.name!r} is off-product")

    def test_every_case_forbids_something(self) -> None:
        """The forbidden sets are the data half of this harness and are easy to delete.

        Without them a path case passes against a script that registers nothing at all,
        because every shell completes filenames on its own — so the case that looks like it
        proves `--out` completes a path proves only that the shell has a filesystem.
        """
        for case in completion_cases():
            self.assertTrue(
                case.forbidden,
                f"case {case.name!r} forbids nothing, so it cannot fail on a wrong candidate",
            )

    def test_a_missing_candidate_fails(self) -> None:
        case = CompletionCase(
            name="dct",
            line=f"{PRODUCT} --dct ",
            expected=("float", "ifast"),
            forbidden=("--quality",),
        )
        with self.assertRaisesRegex(AcceptanceError, r"missing=\['ifast'\]"):
            verify_case("fish", case, "float\n")

    def test_an_unexpected_candidate_fails(self) -> None:
        """A value option that stopped being exclusive offers its values *and* the directory.

        Neither `missing` nor `forbidden` sees that: the expected values are all there and
        the extras are filenames nobody thought to forbid.
        """
        case = CompletionCase(
            name="dct",
            line=f"{PRODUCT} --dct ",
            expected=("float", "ifast", "islow"),
            forbidden=("--quality",),
        )
        with self.assertRaisesRegex(AcceptanceError, r"unexpected=\['page-one.zip'\]"):
            verify_case("fish", case, "float\nifast\nislow\npage-one.zip\n")

    def test_a_forbidden_candidate_fails_even_when_the_expected_ones_are_there(self) -> None:
        """The stock PowerShell regression: a value position offering the option list back."""
        case = CompletionCase(
            name="dct",
            line=f"{PRODUCT} --dct ",
            expected=("float", "ifast", "islow"),
            forbidden=("--quality",),
        )
        with self.assertRaisesRegex(AcceptanceError, r"forbidden=\['--quality'\]"):
            verify_case("powershell", case, "float\nifast\nislow\n--quality\n")

    def test_a_forbidden_string_outside_the_menu_still_fails(self) -> None:
        """`--progressive <Tab>` offering `true` is the defect; the menu parser may miss it."""
        case = CompletionCase(
            name="attached",
            line=f"{PRODUCT} --progressive page",
            expected=("page-one.zip",),
            forbidden=("true", "false"),
        )
        with self.assertRaisesRegex(AcceptanceError, "forbidden="):
            verify_case("bash", case, f"{PRODUCT} --progressive page\n\npage-one.zip  true\n")

    def test_an_attached_value_must_carry_its_option_where_the_shell_replaces_the_word(
        self,
    ) -> None:
        """`false` for `--progressive=` rewrites the line to `comic-auto-resize false`.

        fish and PowerShell return the text that replaces the whole word, so a candidate
        without the option is a different command. An earlier version of this harness stripped
        the prefix when present and accepted its absence, which passed a PowerShell script
        that had dropped it.
        """
        case = CompletionCase(
            name="attached",
            line=f"{PRODUCT} --progressive=",
            expected=("true", "false"),
            forbidden=("page-one.zip",),
            attached="--progressive=",
        )
        self.assertEqual(
            verify_case("powershell", case, "--progressive=true\n--progressive=false\n"),
            ["false", "true"],
        )
        with self.assertRaisesRegex(AcceptanceError, "do not carry"):
            verify_case("powershell", case, "true\nfalse\n")
        # A terminal menu shows the value alone; carrying the option there would insert it
        # twice.
        self.assertEqual(
            verify_case("bash", case, f"{PRODUCT} --progressive=\n\ntrue  false\n"),
            ["false", "true"],
        )
        with self.assertRaisesRegex(AcceptanceError, "carry"):
            verify_case(
                "bash",
                case,
                f"{PRODUCT} --progressive=\n\n--progressive=true  --progressive=false\n",
            )

    def test_a_shell_diagnostic_fails_a_case_that_otherwise_matched(self) -> None:
        case = CompletionCase(
            name="dct",
            line=f"{PRODUCT} --dct ",
            expected=("float",),
            forbidden=("--quality",),
        )
        with self.assertRaisesRegex(AcceptanceError, "diagnostics="):
            verify_case("fish", case, "float\ncompopt: not currently executing\n")

    def test_a_terminal_that_printed_no_menu_yields_no_candidates(self) -> None:
        """Silence must fail a case, not satisfy it.

        A terminal shell that offered nothing echoes the command line and stops, which is
        one block. Reading candidates out of that block would let every case pass on a
        script the shell never sourced.
        """
        self.assertEqual(menu_candidates(f"CAR_PROMPT> {PRODUCT} --r\n"), set())
        case = CompletionCase(
            name="prefix",
            line=f"{PRODUCT} --r",
            expected=("--ratio",),
            forbidden=("--dct",),
        )
        with self.assertRaisesRegex(AcceptanceError, "missing="):
            verify_case("bash", case, f"CAR_PROMPT> {PRODUCT} --r\n")

    def test_a_run_that_skipped_cases_fails(self) -> None:
        """The exit status is all CI reads, so running nothing must not be a fast green."""
        assert_ran("bash", ["syntax", *CASE_ORDER])
        with self.assertRaisesRegex(AcceptanceError, "declared case list"):
            assert_ran("bash", ["syntax"])
        with self.assertRaisesRegex(AcceptanceError, "declared case list"):
            assert_ran("bash", ["syntax", *CASE_ORDER[:-1]])

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

    def test_the_shells_own_state_is_redirected_too(self) -> None:
        """The fixture's promise has to cover what the shells write, not only what we write.

        Measured before this was fixed: a run left `fish` and `powershell` state under the
        ambient cache and data roots, and `bash -n` sourced an ambient `BASH_ENV` inside what
        is supposed to be a syntax check.
        """
        ambient = {
            "XDG_DATA_HOME": "/ambient/data",
            "XDG_CACHE_HOME": "/ambient/cache",
            "XDG_STATE_HOME": "/ambient/state",
            "BASH_ENV": "/ambient/bash_env.sh",
        }
        with mock.patch.dict("os.environ", ambient, clear=False), Fixture("bash") as fixture:
            environment = isolated_environment(fixture, Path("/usr/bin"))
        self.assertNotIn("BASH_ENV", environment)
        for key in ("HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"):
            self.assertTrue(
                Path(environment[key]).is_relative_to(fixture.home),
                f"{key} points outside the fixture at {environment[key]}",
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
            main(
                [
                    "--binary",
                    "/nonexistent/comic-auto-resize",
                    "--shell",
                    "bash",
                    "--shell",
                    "bash",
                ]
            )


if __name__ == "__main__":
    unittest.main()
