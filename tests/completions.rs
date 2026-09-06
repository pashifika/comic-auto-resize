//! Contract tests for the completion entry point, over the shipped binary.
//!
//! These prove properties of the *artefact*: that it is reproducible, that it names the
//! command a shell will invoke, that producing it touches nothing, and that it carries the
//! same flag list `--help` does. What a shell does with the artefact is not provable from
//! here and is not attempted — `.github/scripts/shell_completion_acceptance.py` drives each
//! shell over its own completion machinery, and CI runs it on the target that ships it.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The binary under test, built by Cargo for this integration test at the same profile.
const BINARY: &str = env!("CARGO_BIN_EXE_comic-auto-resize");

/// The name the scripts register a completion for.
const PRODUCT: &str = "comic-auto-resize";

/// Every shell the surface advertises.
const SHELLS: [&str; 4] = ["bash", "zsh", "fish", "powershell"];

/// A temporary directory removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "car-completions-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("creates the scratch directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs `--completions <shell>` from `directory` and returns the script, failing the test
/// with the binary's own diagnostics if it refused.
fn generate_in(directory: &Path, shell: &str) -> Vec<u8> {
    let output = Command::new(BINARY)
        .current_dir(directory)
        .args(["--completions", shell])
        .output()
        .expect("runs the binary");
    assert!(
        output.status.success(),
        "`--completions {shell}` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "`--completions {shell}` wrote to stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.stdout.is_empty(),
        "`--completions {shell}` wrote no script"
    );
    output.stdout
}

/// Every long option `--help` names, without the leading dashes.
///
/// Taken from the help text rather than from the parser because this test runs against the
/// built binary and has no command graph to read. A `--name` in the help prose that is not a
/// flag would make this over-strict — and would already be a violation of the rule that help
/// lists only what exists, so the failure would be the right one.
fn documented_long_options() -> BTreeSet<String> {
    let output = Command::new(BINARY)
        .arg("--help")
        .output()
        .expect("runs the binary");
    assert!(output.status.success(), "`--help` failed");
    let help = String::from_utf8_lossy(&output.stdout);

    let mut options = BTreeSet::new();
    let bytes: Vec<char> = help.chars().collect();
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] == '-' && bytes[index + 1] == '-' && bytes[index + 2].is_alphanumeric() {
            let start = index + 2;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_alphanumeric() || bytes[end] == '-') {
                end += 1;
            }
            options.insert(bytes[start..end].iter().collect::<String>());
            index = end;
        } else {
            index += 1;
        }
    }
    assert!(
        options.contains("help") && options.contains("quality"),
        "the help scan found nothing recognisable: {options:?}"
    );
    options
}

/// How `shell`'s script spells a long option, so a script can be searched for one.
///
/// fish is the odd one: `complete -l jobs` rather than `--jobs`, because the completion is
/// declared to fish rather than written into a case arm.
fn long_option_token(shell: &str, option: &str) -> String {
    if shell == "fish" {
        format!("-l {option}")
    } else {
        format!("--{option}")
    }
}

/// Task 4.1. Two runs produce the same bytes, so a packaged script can be diffed against a
/// regenerated one instead of being trusted.
#[test]
fn generation_is_deterministic() {
    let scratch = TempDir::new("deterministic");
    for shell in SHELLS {
        let first = generate_in(scratch.path(), shell);
        let second = generate_in(scratch.path(), shell);
        assert_eq!(
            first, second,
            "{shell}: two runs produced different scripts"
        );
    }
}

/// Task 4.2. The script binds the name a shell will type, not a placeholder and not whatever
/// the file on this machine happens to be called.
#[test]
fn the_script_binds_the_command_name() {
    let scratch = TempDir::new("bin-name");
    for shell in SHELLS {
        let script =
            String::from_utf8(generate_in(scratch.path(), shell)).expect("the script is UTF-8");
        assert!(
            script.contains(PRODUCT),
            "{shell}: the script never names {PRODUCT}"
        );
    }
}

/// Task 4.3. Generation reads no project state, asserted from a directory holding none.
///
/// A completion script is produced while a shell starts, in whatever directory the user
/// happens to be in. A generator that opened the input, or looked for one, would make an
/// empty directory a broken prompt.
#[test]
fn generation_needs_no_project_state() {
    let scratch = TempDir::new("empty");
    for shell in SHELLS {
        generate_in(scratch.path(), shell);
    }
    let remaining: Vec<_> = fs::read_dir(scratch.path())
        .expect("reads the scratch directory")
        .map(|entry| entry.expect("reads an entry").file_name())
        .collect();
    assert!(
        remaining.is_empty(),
        "generation left files behind: {remaining:?}"
    );
}

/// Task 4.4. The flag list in every script is the flag list in `--help`, because both come
/// from one command graph.
///
/// This is the property the whole Change rests on: a flag added to the parser reaches every
/// shell without a second edit, and a flag cannot be completed without existing.
#[test]
fn every_documented_option_reaches_every_shell() {
    let scratch = TempDir::new("graph");
    let documented = documented_long_options();
    for shell in SHELLS {
        let script =
            String::from_utf8(generate_in(scratch.path(), shell)).expect("the script is UTF-8");
        let missing: Vec<&String> = documented
            .iter()
            .filter(|option| !script.contains(&long_option_token(shell, option)))
            .collect();
        assert!(
            missing.is_empty(),
            "{shell}: --help documents {missing:?}, and the generated script does not offer them"
        );
    }
}

/// Both value enums reach every script, so a user completing `--dct ` is offered the methods
/// rather than a filename.
///
/// Separate from the option test above because an option name appearing is not the same as
/// its values appearing, and the values are what make these two flags worth completing.
#[test]
fn the_value_enums_reach_every_shell() {
    let scratch = TempDir::new("enums");
    for shell in SHELLS {
        let script =
            String::from_utf8(generate_in(scratch.path(), shell)).expect("the script is UTF-8");
        for value in ["islow", "ifast", "float", "nearest-neighbor", "lanczos3"] {
            assert!(
                script.contains(value),
                "{shell}: the script never offers the value {value}"
            );
        }
    }
}

/// The refusal names the shells that exist, so a user who guessed wrong is told what to type.
#[test]
fn an_unrecognised_shell_is_refused_by_name() {
    let output = Command::new(BINARY)
        .args(["--completions", "elvish"])
        .output()
        .expect("runs the binary");
    assert!(
        !output.status.success(),
        "`--completions elvish` was accepted"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for shell in SHELLS {
        assert!(
            stderr.contains(shell),
            "the refusal does not name {shell}: {stderr}"
        );
    }
}

/// A completion request carrying a run's option is refused rather than silently ignoring it.
///
/// The surface's founding rule reaches this flag too: `-q 80 --completions bash` names a
/// quality no script has any use for, and accepting it would be a flag accepted and ignored.
#[test]
fn a_run_option_cannot_ride_along_with_a_completion_request() {
    let output = Command::new(BINARY)
        .args(["-q", "80", "--completions", "bash"])
        .output()
        .expect("runs the binary");
    assert!(
        !output.status.success(),
        "`-q 80 --completions bash` was accepted"
    );
}

/// A completion request needs no input, and every other invocation still does.
#[test]
fn a_bare_invocation_is_refused_naming_the_missing_input() {
    let output = Command::new(BINARY).output().expect("runs the binary");
    assert!(!output.status.success(), "a bare invocation was accepted");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("INPUT"),
        "the refusal does not name the missing input: {stderr}"
    );
}
