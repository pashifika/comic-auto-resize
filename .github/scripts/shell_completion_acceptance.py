#!/usr/bin/env python3
"""Exercise generated completion scripts inside the shells that ship them.

A test that greps a generated script for `--jobs` proves the generator ran. It does not
prove the script parses, that the shell sources it without error, or that pressing Tab
offers `--jobs` — and those are the only properties a user experiences. So each shell is
driven through its own completion machinery and asked what it would offer.

Four kinds of case, one per property the surface has:

  syntax          the shell parses and sources the script without complaint
  option-prefix   an option name completes from a prefix
  enum-values     a value enum's variants complete, for `--dct` and `--resize-mode`
  path            a path completes, for the positional input and for `--out`

Modelled on `skillmount/.github/scripts/shell_completion_acceptance.py` and scaled to this
surface: that CLI has subcommand trees, directory-only and executable-only value hints, and
thirteen cases per shell. This one has a single positional that is an archive or a directory,
one subcommand, and two value enums.
"""

from __future__ import annotations

import argparse
import errno
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Sequence

if os.name != "nt":
    import fcntl
    import pty
    import select
    import struct
    import termios

PRODUCT = "comic-auto-resize"
PROMPT = "CAR_PROMPT> "
SHELL_ORDER = ("bash", "zsh", "fish", "powershell")
CASE_ORDER = (
    "option-prefix",
    "dct-values",
    "resize-mode-values",
    "attached-only-space",
    "attached-only-value",
    "positional-path",
    "out-path",
    "out-path-empty",
)

# Everything a shell writes to a terminal that is not a candidate.
ANSI_ESCAPE = re.compile(rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\)|[@-_])")

# A completion function that failed usually says so on the same line it failed on, and the
# shell still returns zero. These are the phrases that mean the script is broken rather than
# the candidate list being wrong.
SHELL_ERROR_MARKERS = ("unknown match specification", "compopt:", "command not found")

# The two value enums, which are the reason completion is worth more here than a file list.
DCT_METHODS = ("float", "ifast", "islow")
RESIZE_MODES = (
    "nearest-neighbor",
    "bilinear",
    "bicubic",
    "mitchell-netravali",
    "lanczos2",
    "lanczos3",
)

# What the fixture's working directory holds. `zzz.cbz` shares no prefix with the rest, which
# is what makes an empty-prefix case assertable on a terminal shell: with a common prefix the
# first Tab inserts it and the second prints no menu, so the case would read as silence.
PATH_CANDIDATES = ("page-one.zip", "page-two.zip", "pages")
ALL_ENTRIES = PATH_CANDIDATES + ("zzz.cbz",)

class AcceptanceError(RuntimeError):
    """A required observation could not be proved."""


@dataclass(frozen=True)
class CompletionCase:
    name: str
    line: str
    expected: tuple[str, ...]
    # Candidates that must not appear. Every case carries some, because most of the defects
    # this harness exists to catch show up as the *wrong* candidates rather than as missing
    # ones — a value position answered with the option list, a path position answered with
    # `true`/`false`. A case with an empty forbidden set is usually a case that cannot fail:
    # a path case asserting only that files appear passes against a script that registers
    # nothing at all, because every shell completes files on its own.
    forbidden: tuple[str, ...] = field(default=())
    # Removed from each candidate before comparing. The shells disagree about whether an
    # attached value's completion carries the option back: fish and PowerShell answer
    # `--progressive=false`, bash and zsh answer `false`. Both are correct for their shell.
    strip: str = ""


class Fixture:
    """One owned temporary tree; cleanup cannot reach a sibling path."""

    def __init__(self, shell: str, parent: Path | None = None) -> None:
        self._temporary = tempfile.TemporaryDirectory(
            prefix=f"car-completion-{shell}-", dir=parent
        )
        self.root = Path(self._temporary.name)
        self.home = self.root / "home"
        self.work = self.root / "work"
        self.home.mkdir()
        self.work.mkdir()
        (self.work / "page-one.zip").write_bytes(b"PK\x05\x06" + bytes(18))
        (self.work / "page-two.zip").write_bytes(b"PK\x05\x06" + bytes(18))
        (self.work / "zzz.cbz").write_bytes(b"PK\x05\x06" + bytes(18))
        (self.work / "pages").mkdir()

    def __enter__(self) -> Fixture:
        return self

    def __exit__(self, exc_type: object, exc_value: object, traceback: object) -> None:
        self._temporary.cleanup()


def completion_cases() -> tuple[CompletionCase, ...]:
    """The same rows for every shell: one surface, one set of promises.

    Uniform on purpose. Where the shells disagreed it was because the generated script was
    wrong for one of them, so a per-shell expectation would have recorded the defect as the
    contract rather than caught it.
    """
    return (
        CompletionCase(
            name="option-prefix",
            line=f"{PRODUCT} --r",
            expected=("--ratio", "--resize-mode"),
            forbidden=("--dct", "page-one.zip"),
        ),
        CompletionCase(
            name="dct-values",
            line=f"{PRODUCT} --dct ",
            expected=DCT_METHODS,
            forbidden=("--dct", "--quality", "page-one.zip"),
        ),
        CompletionCase(
            name="resize-mode-values",
            line=f"{PRODUCT} --resize-mode ",
            expected=RESIZE_MODES,
            forbidden=("--resize-mode", "--quality", "page-one.zip"),
        ),
        # `--progressive` takes its value only when attached, so the next word is the input
        # path. Every shell offered `true`/`false` there until the guards landed, and picking
        # one produced `error: false: No such file or directory`.
        #
        # The empty prefix is load-bearing: with `--progressive page` a shell that still
        # thinks the next word is the value filters `true`/`false` away against `page` and
        # falls through to files, so the case passes on the defect it exists to catch.
        CompletionCase(
            name="attached-only-space",
            line=f"{PRODUCT} --progressive ",
            expected=ALL_ENTRIES,
            forbidden=("true", "false"),
        ),
        CompletionCase(
            name="attached-only-value",
            line=f"{PRODUCT} --progressive=",
            expected=("true", "false"),
            forbidden=("page-one.zip", "--progressive"),
            strip="--progressive=",
        ),
        # A prefix a shell can filter on. What this proves is that the script does not
        # *hijack* a path position — path completion itself is the shell's own, so a case
        # asserting only that filenames appear would pass against a script that registered
        # nothing at all. The forbidden sets are what carry the assertion.
        CompletionCase(
            name="positional-path",
            line=f"{PRODUCT} page",
            expected=PATH_CANDIDATES,
            forbidden=("--out", "--quality", "true"),
        ),
        CompletionCase(
            name="out-path",
            line=f"{PRODUCT} --out page",
            expected=PATH_CANDIDATES,
            forbidden=("--out", "--quality", "true"),
        ),
        # The empty prefix, which is where a value position is easiest to answer wrongly and
        # where the stock PowerShell body did: `--out <Tab>` returned every option name,
        # `--out` among them, because it filters names by a prefix that matches everything.
        CompletionCase(
            name="out-path-empty",
            line=f"{PRODUCT} --out ",
            expected=ALL_ENTRIES,
            forbidden=("--out", "--quality", "true"),
        ),
    )


def emit(record: dict[str, object]) -> None:
    print(json.dumps(record, sort_keys=True, separators=(",", ":")))


def observation_record(shell: str, case: str, candidates: Sequence[str]) -> dict[str, object]:
    return {
        "shell": shell,
        "case": case,
        "candidates": sorted(candidates),
    }


def decode(value: bytes) -> str:
    return value.decode("utf-8", errors="replace")


def require_interpreter(shell: str) -> tuple[str, str]:
    """Fails closed on an advertised shell the host does not have.

    A skipped shell is the failure this guards: the tool advertises a script for it, so a
    run that quietly proved nothing is worse than a red job.
    """
    executable_name = "pwsh" if shell == "powershell" else shell
    executable = shutil.which(executable_name)
    if executable is None:
        raise AcceptanceError(
            f"required-interpreter: advertised shell {shell!r} is unavailable"
        )
    result = subprocess.run(
        [executable, "--version"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=30,
    )
    if result.returncode != 0:
        raise AcceptanceError(
            f"required-interpreter: {shell} version probe exited "
            f"{result.returncode}: {decode(result.stderr)}"
        )
    return executable, decode(result.stdout or result.stderr).strip().splitlines()[0]


def generate_script(binary: Path, shell: str) -> bytes:
    """Runs the binary's completion entry point, in a directory holding no input.

    Anything on standard error is a failure even when the exit status is zero: a script a
    shell sources at startup must not print, and a warning here would land in the prompt.
    """
    with tempfile.TemporaryDirectory(prefix="car-completion-generate-") as empty:
        result = subprocess.run(
            [str(binary), "--completions", shell],
            check=False,
            cwd=empty,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=60,
        )
    if result.returncode != 0 or result.stderr or not result.stdout:
        raise AcceptanceError(
            f"{binary.name} could not generate the {shell} script: "
            f"exit={result.returncode} stderr={decode(result.stderr)!r}"
        )
    return result.stdout


def isolated_environment(fixture: Fixture, binary_directory: Path) -> dict[str, str]:
    """The shell's configuration is the fixture's, and the binary is on PATH.

    On PATH because a completion is registered against a command name: a shell that cannot
    resolve the name may decline to complete for it at all.

    Every XDG root is redirected, not only the config one. Measured on a developer machine
    with the rest pointing elsewhere, a run created `fish` and `powershell` state under the
    ambient cache and data roots — so the fixture's promise held for what the harness writes
    and not for what the shells write. `BASH_ENV` is removed for a sharper reason: `bash -n`
    sources it before declining to run the script under test, so an ambient one executes
    inside what is supposed to be a syntax check.
    """
    environment = os.environ.copy()
    for leak in ("BASH_ENV", "ENV", "HISTFILE", "SHELLOPTS", "PYTHONSTARTUP"):
        environment.pop(leak, None)
    state = fixture.home / "state"
    state.mkdir(exist_ok=True)
    environment.update(
        {
            "HOME": str(fixture.home),
            "USERPROFILE": str(fixture.home),
            "XDG_CONFIG_HOME": str(fixture.home),
            "XDG_DATA_HOME": str(state),
            "XDG_CACHE_HOME": str(state),
            "XDG_STATE_HOME": str(state),
            "ZDOTDIR": str(fixture.home),
            "TERM": "xterm-256color",
            "PATH": str(binary_directory) + os.pathsep + environment.get("PATH", ""),
        }
    )
    return environment


@dataclass(frozen=True)
class Installation:
    command: tuple[str, ...]
    environment: dict[str, str]
    script: Path


def install_completion(
    shell: str, script: bytes, fixture: Fixture, interpreter: str, binary_directory: Path
) -> Installation:
    """Puts the script where the shell looks for it, and nowhere else."""
    environment = isolated_environment(fixture, binary_directory)
    generated = fixture.home / "generated"
    generated.mkdir()

    if shell == "bash":
        script_path = generated / f"{PRODUCT}.bash"
        script_path.write_bytes(script)
        config = fixture.home / ".bashrc"
        config.write_text(
            f"PS1={shlex.quote(PROMPT)}\n"
            "PROMPT_COMMAND=\n"
            f"source {shlex.quote(str(script_path))}\n",
            encoding="utf-8",
        )
        command = (interpreter, "--noprofile", "--rcfile", str(config), "-i")
    elif shell == "zsh":
        functions = fixture.home / "zsh-functions"
        functions.mkdir()
        script_path = functions / f"_{PRODUCT}"
        script_path.write_bytes(script)
        config = fixture.home / ".zshenv"
        config.write_text(
            f"fpath=({shlex.quote(str(functions))} $fpath)\n"
            "autoload -U +X compinit && compinit -u -d $ZDOTDIR/.zcompdump\n"
            'precmd_functions=""\n'
            f"PS1={shlex.quote(PROMPT)}\n"
            f"PROMPT={shlex.quote(PROMPT)}\n",
            encoding="utf-8",
        )
        command = (interpreter, "--noglobalrcs", "-i")
    elif shell == "fish":
        script_path = fixture.home / "fish" / "completions" / f"{PRODUCT}.fish"
        script_path.parent.mkdir(parents=True)
        script_path.write_bytes(script)
        command = (interpreter,)
    elif shell == "powershell":
        script_path = generated / f"{PRODUCT}.ps1"
        script_path.write_bytes(script)
        command = (interpreter, "-NoLogo", "-NoProfile", "-NonInteractive")
    else:
        raise AcceptanceError(f"unsupported harness shell {shell!r}")

    return Installation(command, environment, script_path)


def syntax_check(shell: str, installation: Installation) -> None:
    """The `syntax` case: the shell parses the script without executing it."""
    if shell in ("bash", "zsh"):
        command = [installation.command[0], "-n", str(installation.script)]
    elif shell == "fish":
        command = [installation.command[0], "--no-execute", str(installation.script)]
    else:
        command = [
            installation.command[0],
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            (
                "$tokens=$null; $errors=$null; "
                "[System.Management.Automation.Language.Parser]::ParseFile("
                "$env:CAR_COMPLETION_SCRIPT,[ref]$tokens,[ref]$errors) > $null; "
                "if ($errors.Count -ne 0) { $errors | Out-String | Write-Error; exit 1 }"
            ),
        ]
    environment = installation.environment.copy()
    environment["CAR_COMPLETION_SCRIPT"] = str(installation.script)
    result = subprocess.run(
        command,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=environment,
        timeout=60,
    )
    if result.returncode != 0:
        raise AcceptanceError(
            f"{shell}: the generated script does not parse: "
            f"{decode(result.stderr or result.stdout)}"
        )


# ------------------------------------------------------------------ terminal shells


def normalized_terminal_output(value: bytes) -> str:
    """What a person would see: escapes removed and backspaces applied."""
    value = ANSI_ESCAPE.sub(b"", value)
    output: list[str] = []
    for character in decode(value):
        if character == "\b":
            if output:
                output.pop()
        elif character == "\r":
            output.append("\n")
        elif character == "\a" or (ord(character) < 32 and character not in "\n\t"):
            continue
        else:
            output.append(character)
    return "".join(output)


def _read_available(master: int, timeout: float) -> bytes:
    ready, _, _ = select.select([master], [], [], timeout)
    if not ready:
        return b""
    try:
        return os.read(master, 65536)
    except OSError as error:
        if error.errno == errno.EIO:
            return b""
        raise


def _wait_for_prompt(master: int, process: subprocess.Popen[bytes]) -> None:
    observed = bytearray()
    deadline = time.monotonic() + 20
    marker = PROMPT.encode()
    while time.monotonic() < deadline:
        observed.extend(_read_available(master, 0.2))
        if marker in observed:
            return
        if process.poll() is not None:
            break
    raise AcceptanceError(
        "the interactive shell never reached its prompt, so the script did not source: "
        + normalized_terminal_output(bytes(observed))
    )


def _collect_completion(master: int) -> bytes:
    observed = bytearray()
    started = time.monotonic()
    last_data = started
    while time.monotonic() - started < 6:
        chunk = _read_available(master, 0.1)
        if chunk:
            observed.extend(chunk)
            last_data = time.monotonic()
        now = time.monotonic()
        if now - started >= 0.8 and now - last_data >= 0.4:
            break
    return bytes(observed)


def interactive_completion(installation: Installation, fixture: Fixture, line: str) -> str:
    """Types `line` and two tabs into a real terminal and reads back what appears.

    Two tabs rather than one: a shell completes the common prefix on the first and lists
    the candidates on the second, and it is the list this asserts against.
    """
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 80, 512, 0, 0))

    def configure_controlling_terminal() -> None:
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

    process = subprocess.Popen(
        installation.command,
        cwd=fixture.work,
        env=installation.environment,
        stdin=slave,
        stdout=slave,
        stderr=slave,
        close_fds=True,
        preexec_fn=configure_controlling_terminal,
    )
    os.close(slave)
    try:
        _wait_for_prompt(master, process)
        os.write(master, line.encode() + b"\t\t")
        return normalized_terminal_output(_collect_completion(master))
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        os.close(master)


# ------------------------------------------------------- shells with a completion API

FISH_COMPLETE = """
source "$CAR_COMPLETION_SCRIPT"
cd "$CAR_COMPLETION_WORK"
complete --do-complete="$CAR_COMPLETION_LINE"
"""

POWERSHELL_COMPLETE = r"""
$ErrorActionPreference = 'Stop'
. $env:CAR_COMPLETION_SCRIPT
Set-Location -LiteralPath $env:CAR_COMPLETION_WORK
$line = $env:CAR_COMPLETION_LINE
$completion = [System.Management.Automation.CommandCompletion]::CompleteInput(
    $line, $line.Length, $null
)
$completion.CompletionMatches | ForEach-Object { $_.CompletionText }
"""


def _api_completion(
    installation: Installation, fixture: Fixture, line: str, command: list[str]
) -> str:
    environment = installation.environment.copy()
    environment.update(
        {
            "CAR_COMPLETION_SCRIPT": str(installation.script),
            "CAR_COMPLETION_WORK": str(fixture.work),
            "CAR_COMPLETION_LINE": line,
        }
    )
    result = subprocess.run(
        command,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=environment,
        timeout=60,
    )
    if result.returncode != 0:
        raise AcceptanceError(
            f"completion query failed: {decode(result.stderr or result.stdout)}"
        )
    return decode(result.stdout)


def fish_completion(installation: Installation, fixture: Fixture, line: str) -> str:
    return _api_completion(
        installation,
        fixture,
        line,
        [installation.command[0], "--no-config", "--command", FISH_COMPLETE],
    )


def powershell_completion(installation: Installation, fixture: Fixture, line: str) -> str:
    return _api_completion(
        installation, fixture, line, [*installation.command, "-Command", POWERSHELL_COMPLETE]
    )


# ------------------------------------------------------------------------- verdict


def normalize_candidate(value: str) -> str:
    """One candidate as the user would read it: no description, no path, no marker.

    Both separators, because one case list runs on both release targets: PowerShell answers
    `./page-one.zip` on macOS and `.\\page-one.zip` on Windows, and a set that handled only
    one of them would compare a path against a filename.
    """
    candidate = value.split("\t", 1)[0].rstrip("\r\n").strip()
    candidate = candidate.removesuffix("*").rstrip("/\\")
    for separator in ("/", "\\"):
        if separator in candidate:
            candidate = candidate.rsplit(separator, 1)[-1]
    return candidate


def machine_candidates(observed: str) -> set[str]:
    """One candidate per line, which is what fish and PowerShell return."""
    return {
        candidate for line in observed.splitlines() if (candidate := normalize_candidate(line))
    }


def menu_candidates(observed: str) -> set[str]:
    """The candidate menu a terminal shell printed under the echoed command line.

    The first block is the echoed line; everything after it is the menu. A shell that
    printed no menu leaves one block, and the empty set that produces is what makes the
    case fail rather than pass silently.
    """
    blocks: list[list[str]] = []
    block: list[str] = []
    for line in observed.splitlines():
        if line.strip():
            block.append(line)
        elif block:
            blocks.append(block)
            block = []
    if block:
        blocks.append(block)
    if len(blocks) < 2:
        return set()

    candidates: set[str] = set()
    for menu in blocks[1:]:
        for line in menu:
            if line.startswith(PROMPT) or line.lstrip().startswith(PRODUCT):
                continue
            # zsh writes `candidate  -- description`; take the candidate side only.
            for value in line.split(" -- ", 1)[0].split():
                if candidate := normalize_candidate(value):
                    candidates.add(candidate)
    return candidates


def verify_case(shell: str, case: CompletionCase, observed: str) -> list[str]:
    """Compares what the shell offered against what the case requires.

    Three ways to fail, and the third is the one worth spelling out. Missing candidates are
    obvious. Forbidden candidates catch a value position answered with the option list.
    **Unexpected** candidates catch the shape neither of those sees: a value option that
    stopped being exclusive, so the shell offers its values *and* the directory listing.
    Dropping that check let a fish `--dct <Tab>` answering
    `float ifast islow page-one.zip page-two.zip pages` pass.

    A shell diagnostic fails the case too: a completion function that errored mid-way can
    still leave the expected candidates on screen, and its message would otherwise be
    recorded in the evidence as though the words of it were candidates.
    """
    expected = {normalize_candidate(value) for value in case.expected}
    forbidden = {normalize_candidate(value) for value in case.forbidden}
    diagnostics = [marker for marker in SHELL_ERROR_MARKERS if marker in observed]

    if shell in ("fish", "powershell"):
        actual = machine_candidates(observed)
    else:
        actual = menu_candidates(observed)
    if case.strip:
        actual = {
            candidate.removeprefix(case.strip) if candidate.startswith(case.strip) else candidate
            for candidate in actual
        }

    missing = sorted(expected - actual)
    unexpected = sorted(actual - expected)
    # The raw scan catches a forbidden string the menu parser did not turn into a candidate.
    # It skips anything the case's own line contains, because a terminal echoes the line it
    # was given and `--dct <Tab>` would otherwise be forbidden from mentioning `--dct`.
    offered_forbidden = sorted(
        (actual & forbidden)
        | {value for value in case.forbidden if value not in case.line and value in observed}
    )
    if missing or unexpected or offered_forbidden or diagnostics:
        raise AcceptanceError(
            f"case {case.name!r} failed in {shell}: missing={missing!r} "
            f"unexpected={unexpected!r} forbidden={offered_forbidden!r} "
            f"diagnostics={diagnostics!r} actual={sorted(actual)!r} observed={observed!r}"
        )
    return sorted(actual)


def run_shell(binary: Path, shell: str) -> None:
    interpreter, version = require_interpreter(shell)
    script = generate_script(binary, shell)
    emit({"shell": shell, "interpreter": interpreter, "version": version, "bytes": len(script)})

    observed_cases: list[str] = []
    with Fixture(shell) as fixture:
        installation = install_completion(shell, script, fixture, interpreter, binary.parent)
        syntax_check(shell, installation)
        emit({"shell": shell, "case": "syntax", "candidates": []})
        observed_cases.append("syntax")

        for case in completion_cases():
            if shell == "fish":
                observed = fish_completion(installation, fixture, case.line)
            elif shell == "powershell":
                observed = powershell_completion(installation, fixture, case.line)
            else:
                observed = interactive_completion(installation, fixture, case.line)
            emit(observation_record(shell, case.name, verify_case(shell, case, observed)))
            observed_cases.append(case.name)

    assert_ran(shell, observed_cases)


def assert_ran(shell: str, observed_cases: Sequence[str]) -> None:
    """The exit status is all CI reads, so a harness that ran no case would report success.

    Comparing what was emitted against the declared list is what makes running nothing a
    failure rather than a fast green.
    """
    if tuple(observed_cases) != ("syntax", *CASE_ORDER):
        raise AcceptanceError(
            f"{shell}: ran {list(observed_cases)!r}, which is not the declared case list "
            f"{['syntax', *CASE_ORDER]!r}"
        )


def verify_binary(binary: Path) -> Path:
    binary = binary.resolve(strict=True)
    result = subprocess.run(
        [str(binary), "--version"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=60,
    )
    if result.returncode != 0:
        raise AcceptanceError(
            f"{binary} is not runnable: {decode(result.stderr or result.stdout)}"
        )
    return binary


def parse_args(arguments: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Prove generated completions in real shells.")
    parser.add_argument("--binary", required=True, type=Path, help="the built executable")
    parser.add_argument(
        "--shell",
        action="append",
        dest="shells",
        required=True,
        choices=SHELL_ORDER,
        help="a shell to exercise; repeat for more",
    )
    return parser.parse_args(arguments)


def main(arguments: Sequence[str] | None = None) -> int:
    options = parse_args(sys.argv[1:] if arguments is None else arguments)

    # Named once each, so a copy-pasted invocation cannot report one shell twice and look
    # like it covered two.
    duplicates = sorted({shell for shell in options.shells if options.shells.count(shell) > 1})
    if duplicates:
        raise AcceptanceError(f"each shell must be named exactly once; repeated: {duplicates}")

    if os.name == "nt" and set(options.shells) - {"powershell"}:
        raise AcceptanceError("the terminal-driven shells need a pty, which Windows has not")

    binary = verify_binary(options.binary)
    for shell in SHELL_ORDER:
        if shell in options.shells:
            run_shell(binary, shell)

    emit({"result": "ok", "shells": sorted(options.shells)})
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except AcceptanceError as error:
        print(f"::error::{error}", file=sys.stderr)
        raise SystemExit(1) from error
