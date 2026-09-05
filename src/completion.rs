//! Shell-completion scripts, generated from the shared command graph.
//!
//! Generated rather than written. A hand-written script is a second copy of the flag list,
//! free to drift from the first; produced from the same `clap::Command` the parser and
//! `--help` are produced from, a flag cannot appear in completion without existing.
//!
//! Nothing here opens the input or reads filesystem state. A completion script is produced
//! while a shell starts, where a failure is a broken prompt rather than a failed command.

use std::collections::BTreeSet;
use std::io::{self, Write};

use clap::{Arg, Command, ValueEnum};
use clap_complete::aot::{Bash, Fish, PowerShell, Zsh, generate};

/// The shells the two release targets use: bash, zsh and fish for `aarch64-apple-darwin`,
/// PowerShell for `x86_64-pc-windows-msvc`.
///
/// Four rather than `clap_complete::Shell`'s five: elvish ships with neither release target,
/// and CI proves a script in the shell it targets, so advertising a fifth would advertise
/// something unproven. `clap` refuses an unrecognised name against this list, which is what
/// makes the refusal name the shells that exist.
///
/// The variants carry no doc comments on purpose. `clap` turns a variant's doc into per-value
/// help, which gives the option a long-help form, which switches `--help` from the two-column
/// layout to the vertical one — for *every* option on the surface. Naming which host ships
/// which shell is not worth reformatting the whole help to say.
// `PowerShell` ends with the enum's name, which the lint reads as a stutter. Here it is the
// shell's own name: spelling it `Power` to satisfy the lint would rename a product. `expect`
// rather than `allow`, so dropping the variant is told the exemption is no longer needed.
#[expect(
    clippy::enum_variant_names,
    reason = "PowerShell is what the shell is called"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Shell {
    Bash,
    Zsh,
    Fish,
    // Without this the derive's kebab-case rename would spell the variant `power-shell`,
    // which is not what the shell is called.
    #[value(name = "powershell")]
    PowerShell,
}

/// Renders `shell`'s script for the shared command graph.
///
/// Into memory rather than straight to the destination: `clap_complete`'s generators panic on
/// a write error, so handing them standard output would turn a reader that closed the pipe
/// into an abort. Writing to a `Vec` cannot fail, which leaves exactly one fallible write —
/// the one in [`write`], which gets to decide what a closed pipe means.
fn script(shell: Shell) -> Vec<u8> {
    let mut command = crate::command();
    // The name the shell will complete for, taken from the graph rather than from `argv[0]`:
    // a script registers against a command name, and the name a package manager installs is
    // this one whatever the file on the developer's machine happens to be called.
    let name = command.get_name().to_string();
    command.build();
    let mut script = Vec::new();
    match shell {
        Shell::Bash => generate(Bash, &mut command, name, &mut script),
        Shell::Zsh => generate(Zsh, &mut command, name, &mut script),
        Shell::Fish => generate(Fish, &mut command, name, &mut script),
        Shell::PowerShell => {
            generate(PowerShell, &mut command, name, &mut script);
            script = guarded_powershell(&script, &command);
        }
    }
    script
}

/// Where the guard below is spliced into `clap_complete`'s PowerShell output: after the
/// generated preamble has resolved `$command` and before it dispatches on it.
const POWERSHELL_ANCHOR: &str = "    $completions = @(switch ($command) {";

/// The value dispatch `clap_complete` 4.6.9's PowerShell generator does not emit.
///
/// Measured rather than inherited, which is the whole reason this exists. Its stock output is
/// correct here for option names and for paths — `--r` offers `--ratio` and `--resize-mode`,
/// and the positional and `--out` both complete files through PowerShell's own fallback — and
/// carries no `PossibleValue` at all. So without this, `--dct <Tab>` offers the option list
/// back, `--dct` among it. The other three shells need nothing: bash, zsh and fish each emit
/// their possible values unaided.
///
/// `skillmount` replaces this generator outright, several hundred lines of it, because its
/// arguments carry `ValueHint::DirPath` and `ValueHint::ExecutablePath` and the stock path
/// handling is wrong for both. This surface has neither hint — the positional is an archive
/// *or* a directory and `--out` is a location *or* a filename, both ordinary file completion —
/// so the gap here is narrower than the one there, and this is a guard over the generated
/// bytes rather than a fork of the generator. An upstream release that emits values shrinks
/// this to nothing instead of leaving a fork to reconcile.
fn guarded_powershell(generated: &[u8], command: &Command) -> Vec<u8> {
    let generated = std::str::from_utf8(generated).expect("clap_complete emits UTF-8");
    // A missing anchor means the pinned generator changed shape, which cannot happen without
    // an edit to `Cargo.toml`: the version is exact. The contract tests generate this script,
    // so a bump that moved the anchor would fail in CI rather than in someone's prompt.
    let split = generated
        .find(POWERSHELL_ANCHOR)
        .expect("clap_complete 4.6.9's PowerShell body dispatches on $command");

    let mut guarded = String::with_capacity(generated.len() * 2);
    guarded.push_str(&generated[..split]);
    guarded.push_str(POWERSHELL_GUARD_HEAD);
    powershell_option_values(&mut guarded, command);
    guarded.push_str(POWERSHELL_GUARD_TAIL);
    guarded.push_str(&generated[split..]);
    guarded.into_bytes()
}

/// Resolves which option's value is being completed, then dispatches on it.
///
/// Two spellings, because both reach this surface: a value in the next word (`--dct ifa`) and
/// a value attached with `=` (`--progressive=fa`), which is the only spelling `--progressive`
/// and `--optimizer` accept at all.
const POWERSHELL_GUARD_HEAD: &str = r#"    # Value completions. clap_complete's PowerShell generator emits option names and
    # subcommand names and no possible values, so without this `--dct <Tab>` offers the
    # option list again. Everything it does emit is left to the generated body below.
    $carOption = $null
    $carPrefix = $wordToComplete
    $carAttached = ''
    if ($wordToComplete -match '^(--[^=]+)=(.*)$') {
        $carOption = $Matches[1]
        $carPrefix = $Matches[2]
        $carAttached = "$($Matches[1])="
    } else {
        $carPreceding = @($commandElements | Where-Object { $_.Extent.EndOffset -lt $cursorPosition })
        if ($carPreceding.Count -gt 1 -and $carPreceding[-1].Extent.Text.StartsWith('-')) {
            $carOption = $carPreceding[-1].Extent.Text
        }
    }
    $carValues = switch ($carOption) {
"#;

/// Emits the matches, or falls through to the generated body when there are none.
///
/// Falling through rather than returning an empty set is what keeps the guard from hiding a
/// completion: `--dct -<Tab>` names no method, and the generated body below still offers the
/// option list.
const POWERSHELL_GUARD_TAIL: &str = r#"        default { $null }
    }
    if ($null -ne $carValues) {
        $carMatches = @(@($carValues) |
            Where-Object { $_.StartsWith($carPrefix, [System.StringComparison]::OrdinalIgnoreCase) } |
            Sort-Object)
        if ($carMatches.Count -gt 0) {
            $carMatches | ForEach-Object {
                [CompletionResult]::new("$carAttached$_", $_, [CompletionResultType]::ParameterValue, $_)
            }
            return
        }
    }

"#;

/// One arm per option that has possible values, keyed by the option's own spelling.
///
/// Keyed on the option alone rather than on a command path because this surface has no
/// subcommands and, per the design, will not grow one: the completion entry point is a flag.
fn powershell_option_values(out: &mut String, command: &Command) {
    for argument in command.get_opts().filter(|arg| !arg.is_hide_set()) {
        let values = possible_values(argument);
        if values.is_empty() {
            continue;
        }
        for option in option_names(argument) {
            write_powershell_arm(out, 8, &option, &values);
        }
    }
}

/// Every spelling an option answers to, long and short.
fn option_names(argument: &Arg) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    if let Some(shorts) = argument.get_short_and_visible_aliases() {
        names.extend(shorts.into_iter().map(|short| format!("-{short}")));
    }
    if let Some(longs) = argument.get_long_and_visible_aliases() {
        names.extend(longs.into_iter().map(|long| format!("--{long}")));
    }
    names
}

/// An argument's visible possible values, sorted so the script is byte-stable.
fn possible_values(argument: &Arg) -> BTreeSet<String> {
    argument
        .get_possible_values()
        .into_iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| value.get_name().to_owned())
        .collect()
}

/// `'<key>' { @('a', 'b'); break }`, indented.
fn write_powershell_arm(out: &mut String, indent: usize, key: &str, values: &BTreeSet<String>) {
    for _ in 0..indent {
        out.push(' ');
    }
    write_powershell_literal(out, key);
    out.push_str(" { @(");
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            out.push_str(", ");
        }
        write_powershell_literal(out, value);
    }
    out.push_str("); break }\n");
}

/// A single-quoted PowerShell literal, where a quote is escaped by doubling it.
fn write_powershell_literal(out: &mut String, value: &str) {
    out.push('\'');
    let mut parts = value.split('\'');
    if let Some(first) = parts.next() {
        out.push_str(first);
    }
    for part in parts {
        out.push_str("''");
        out.push_str(part);
    }
    out.push('\'');
}

/// Writes `shell`'s script, treating a reader that closed the pipe as success.
///
/// `comic-auto-resize completions bash | head` is an ordinary thing for a person to do, and
/// the shell that pipes a script into `source` at startup is doing the same shape of thing.
/// Every other write failure is reported: a script truncated by a full disk is not one a
/// shell should source.
///
/// # Errors
///
/// Any write or flush failure other than a closed pipe.
pub(crate) fn write(shell: Shell, writer: &mut dyn Write) -> io::Result<()> {
    let script = script(shell);
    match writer.write_all(&script).and_then(|()| writer.flush()) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::{Shell, write};
    use std::io::{self, Write};

    /// A destination that fails every write with one chosen error.
    struct Failing(io::ErrorKind);

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(self.0, "test destination"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(self.0, "test destination"))
        }
    }

    /// A reader that closed the pipe is not a failure.
    ///
    /// Driven through a destination rather than through a real pipe deliberately: every
    /// generated script here is a few kilobytes and a pipe buffer is 64 KiB, so a spawned
    /// `| head` never makes the writer see `EPIPE` and the end-to-end version of this test
    /// would pass without exercising anything.
    #[test]
    fn a_closed_pipe_is_success() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell] {
            let mut destination = Failing(io::ErrorKind::BrokenPipe);
            assert!(
                write(shell, &mut destination).is_ok(),
                "{shell:?}: a closed pipe should be success"
            );
        }
    }

    /// Every other write failure still is one, so the clause above cannot swallow a
    /// truncated script.
    #[test]
    fn a_write_that_failed_for_another_reason_is_reported() {
        let mut destination = Failing(io::ErrorKind::StorageFull);
        let error = write(Shell::Bash, &mut destination).expect_err("should have failed");
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
    }
}
