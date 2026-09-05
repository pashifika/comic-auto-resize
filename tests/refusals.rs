//! What the tool refuses, over the whole finished surface.
//!
//! Separate from `tests/pipeline.rs`, which is about a run that works. These are the runs that
//! do not, and they need flags from four Changes at once — `-o` and `--delete-org` from one,
//! `-r` and `--jobs` from another, `--progressive` from a third, `--completions` from a fourth
//! — so no per-flag test could state a rule that holds across all of them.
//!
//! Three rules are pinned here. A refusal's exit code is decided by the kind of fault rather
//! than by the layer that caught it. A refusal names the path it is about, including the ones
//! the run itself cannot name. And an option fault is refused before the input is touched,
//! which is observable only by presenting two faults at once and seeing which wins.

mod support;

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use comic_auto_resize::page::{DctMethod, Filter};
use comic_auto_resize::sink::InputKind;
use support::{TempDir, page_bytes, write_archive, write_pages, write_tree};

/// The binary under test, built by Cargo for this integration test at the same profile.
const BINARY: &str = env!("CARGO_BIN_EXE_comic-auto-resize");

/// A fault the user can fix from the command line alone, refused by the parser.
const USAGE: i32 = 2;
/// A fault that needed the filesystem or the input's contents.
const RUNTIME: i32 = 1;

/// Runs the binary and returns its exit code and stderr.
///
/// `code` rather than `success`: which code a script sees is the property under test, and
/// `ExitStatus::code` is `None` only for a signal, which is a failure of the test rather than
/// a refusal.
fn refusal(args: &[&OsStr]) -> (i32, String) {
    let output = std::process::Command::new(BINARY)
        .args(args)
        .output()
        .expect("runs the binary");
    let code = output
        .status
        .code()
        .expect("the binary exited rather than being signalled");
    (code, String::from_utf8_lossy(&output.stderr).into_owned())
}

/// The output path a file input gets when `-o` says nothing.
fn default_output(input: &Path) -> PathBuf {
    comic_auto_resize::sink::default_output(input, InputKind::File)
        .expect("a file input always has a name")
}

/// A valid input, so a refusal is attributable to the flag rather than to the archive.
fn valid_input(directory: &TempDir) -> PathBuf {
    let path = directory.join("in.zip");
    write_pages(&path, 1, 320, 440);
    path
}

/// An entry whose leading bytes claim JPEG and whose body is not one.
///
/// The signature probe accepts it — that is the point, since a page that the *reader* refuses
/// is a source failure and reaches the user by a different route — and the decoder does not.
fn undecodable_page() -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
    bytes.resize(256, 0);
    bytes
}

/// An archive holding one entry that is not a page.
fn without_pages(directory: &TempDir) -> PathBuf {
    let path = directory.join("no-pages.zip");
    write_archive(
        &path,
        &[("readme.txt".to_owned(), b"no pages in here".to_vec())],
    );
    path
}

/// An archive whose only page cannot be decoded.
fn with_an_undecodable_page(directory: &TempDir) -> PathBuf {
    let path = directory.join("bad-page.zip");
    write_archive(&path, &[("001.jpg".to_owned(), undecodable_page())]);
    path
}

/// An archive whose two entries become one name once the extension is rewritten.
fn with_colliding_names(directory: &TempDir) -> PathBuf {
    let path = directory.join("collide.zip");
    let page = page_bytes(320, 440);
    write_archive(
        &path,
        &[
            ("cover.jpeg".to_owned(), page.clone()),
            ("cover.jpg".to_owned(), page),
        ],
    );
    path
}

/// Which exit code a script sees is a property of the fault, not of the layer that caught it.
///
/// The split is `sysexits`': a refusal the user can act on from the command line alone exits 2
/// with the parser's rendering, and one that needed the filesystem or the input's contents
/// exits 1. Nothing moved between the tiers to make this pass — the build already did this,
/// and what is new is that a validation hand-rolled inside `run()` instead of at the parser
/// can no longer quietly move a usage fault from 2 to 1.
#[test]
fn a_usage_fault_exits_two_and_a_runtime_fault_exits_one() {
    let directory = TempDir::new("tiers");
    let input = valid_input(&directory);
    let ceiling = usize::MAX.to_string();

    let usage: Vec<Vec<&OsStr>> = vec![
        // A flag that does not exist.
        vec!["--nope".as_ref(), input.as_ref()],
        // A value outside its range, at both ends and on three flags.
        vec!["--quality".as_ref(), "0".as_ref(), input.as_ref()],
        vec!["--quality".as_ref(), "101".as_ref(), input.as_ref()],
        vec!["--ratio".as_ref(), "0".as_ref(), input.as_ref()],
        vec!["--ratio".as_ref(), "250".as_ref(), input.as_ref()],
        vec!["--auto-width".as_ref(), "0".as_ref(), input.as_ref()],
        vec!["--auto-width".as_ref(), "65536".as_ref(), input.as_ref()],
        vec!["--jobs".as_ref(), "0".as_ref(), input.as_ref()],
        vec!["--jobs".as_ref(), ceiling.as_ref(), input.as_ref()],
        // A value outside a fixed set.
        vec!["--dct".as_ref(), "fast".as_ref(), input.as_ref()],
        vec!["--resize-mode".as_ref(), "nearest".as_ref(), input.as_ref()],
        vec!["--progressive=maybe".as_ref(), input.as_ref()],
        vec!["--completions".as_ref(), "tcsh".as_ref()],
        // A value the parser can see is unusable without asking the filesystem.
        vec!["-o".as_ref(), "".as_ref(), input.as_ref()],
        // Two flags naming one quantity, from either side.
        vec![
            "--ratio".as_ref(),
            "30".as_ref(),
            "--auto-width".as_ref(),
            "1000".as_ref(),
            input.as_ref(),
        ],
        vec![
            "--auto-width".as_ref(),
            "1000".as_ref(),
            "--ratio".as_ref(),
            "30".as_ref(),
            input.as_ref(),
        ],
        // A completion request accepts nothing else, and the positional is required for
        // everything that is not one.
        vec!["--completions".as_ref(), "bash".as_ref(), input.as_ref()],
        vec![],
    ];
    for args in usage {
        let (code, message) = refusal(&args);
        assert_eq!(code, USAGE, "{args:?} exited {code}: {message}");
        assert!(
            !default_output(&input).exists(),
            "{args:?} produced an output archive"
        );
    }

    let missing = directory.join("not-here.zip");
    let decoy = directory.join("decoy.zip");
    fs::write(&decoy, b"this is not an archive").expect("writes the decoy");
    let taken = directory.join("taken.zip");
    fs::write(&taken, b"already here").expect("writes the obstacle");
    let nowhere = directory.join("nowhere").join("out.zip");
    let tree = directory.join("pages");
    write_tree(&tree, &[("001.jpg", page_bytes(320, 440))]);
    let empty = without_pages(&directory);
    let undecodable = with_an_undecodable_page(&directory);

    let runtime: Vec<Vec<&OsStr>> = vec![
        vec![missing.as_ref()],
        vec![decoy.as_ref()],
        vec![empty.as_ref()],
        vec![undecodable.as_ref()],
        vec!["-o".as_ref(), taken.as_ref(), input.as_ref()],
        vec!["-o".as_ref(), nowhere.as_ref(), input.as_ref()],
        vec!["--delete-org".as_ref(), tree.as_ref()],
    ];
    for args in runtime {
        let (code, message) = refusal(&args);
        assert_eq!(code, RUNTIME, "{args:?} exited {code}: {message}");
    }
}

/// The tier matrix is not passing because every value is refused.
///
/// Each of these is the accepted side of a boundary the rows above refuse, so a build that
/// refused its own range would fail here rather than look correct there.
#[test]
fn a_value_inside_its_range_is_accepted() {
    for args in [
        vec!["--quality", "1"],
        vec!["--quality", "100"],
        vec!["--auto-width", "65535"],
        vec!["--ratio", "1"],
        vec!["--ratio", "100"],
        vec!["--jobs", "1"],
        vec!["--dct", "islow"],
        vec!["--resize-mode", "nearest-neighbor"],
        vec!["--progressive=false"],
    ] {
        let directory = TempDir::new("in-range");
        let input = valid_input(&directory);
        let status = std::process::Command::new(BINARY)
            .args(&args)
            .arg(&input)
            .status()
            .expect("runs the binary");
        assert!(status.success(), "{args:?} was refused but is in range");
    }
}

/// A missing input and a file that is not an archive are named, and the second says what the
/// build reads.
///
/// "Not a zip archive" stopped being the whole answer when rar arrived, so the refusal names
/// the formats rather than the one it was written for.
#[test]
fn a_missing_input_and_a_non_archive_are_named() {
    let directory = TempDir::new("bad-input");

    let missing = directory.join("nope.zip");
    let (code, message) = refusal(&[missing.as_ref()]);
    assert_eq!(code, RUNTIME, "{message}");
    assert!(
        message.contains("nope.zip"),
        "stderr must name the path: {message}"
    );
    assert!(!default_output(&missing).exists());

    let not_zip = directory.join("plain.zip");
    fs::write(&not_zip, b"this is not an archive").expect("writes the decoy");
    let (code, message) = refusal(&[not_zip.as_ref()]);
    assert_eq!(code, RUNTIME, "{message}");
    assert!(
        message.contains("not an archive this build reads"),
        "stderr must say why: {message}"
    );
    assert!(
        message.contains("zip") && message.contains("rar"),
        "stderr must name the formats this build reads: {message}"
    );
    assert!(
        message.contains(&not_zip.display().to_string()),
        "stderr must name the path: {message}"
    );
    assert!(!default_output(&not_zip).exists());
}

/// A failure the run cannot name for itself is named by the presenter.
///
/// `pipeline::run` is handed a source rather than a path, so these three said only what they
/// knew: "no pages to process" with no path at all, and a page failure or a name collision
/// naming the *entry*. A user working through a shelf of archives was told a page failed and
/// not which book it was in. The entry is still named where there is one — the archive's name
/// is added in front of it, not instead of it.
#[test]
fn a_failure_about_the_input_names_the_input() {
    let directory = TempDir::new("named-subjects");

    let empty = without_pages(&directory);
    let (code, message) = refusal(&[empty.as_ref()]);
    assert_eq!(code, RUNTIME, "{message}");
    assert!(
        message.contains(&empty.display().to_string()),
        "an empty input was not named: {message}"
    );
    assert!(message.contains("no pages to process"), "{message}");

    let undecodable = with_an_undecodable_page(&directory);
    let (code, message) = refusal(&[undecodable.as_ref()]);
    assert_eq!(code, RUNTIME, "{message}");
    assert!(
        message.contains(&undecodable.display().to_string()) && message.contains("001.jpg"),
        "a page failure must name the archive and the entry: {message}"
    );

    let collision = with_colliding_names(&directory);
    let (code, message) = refusal(&[collision.as_ref()]);
    assert_eq!(code, RUNTIME, "{message}");
    assert!(
        message.contains(&collision.display().to_string()) && message.contains("cover.jpg"),
        "a name collision must name the archive and the name: {message}"
    );
}

/// A failure about the output names the output, and not the input as well.
///
/// This is why the input's name is not prefixed centrally: `{input}: {output}: already exists`
/// would be two unlabelled paths in a sentence whose subject was never the input.
#[test]
fn a_failure_about_the_output_does_not_name_the_input_instead() {
    let directory = TempDir::new("output-subjects");
    let input = valid_input(&directory);
    let taken = directory.join("taken.zip");
    fs::write(&taken, b"already here").expect("writes the obstacle");

    let (code, message) = refusal(&["-o".as_ref(), taken.as_ref(), input.as_ref()]);
    assert_eq!(code, RUNTIME, "{message}");
    assert!(
        message.contains(&taken.display().to_string()) && message.contains("already exists"),
        "the refusal must name the output: {message}"
    );
    assert!(
        !message.contains(&input.display().to_string()),
        "the refusal named the input, which is not what it is about: {message}"
    );
}

/// With two faults present at once, the option's refusal wins and the input is never opened.
///
/// The test every other refusal test could not be: they all use a *valid* input, so they can
/// assert only that no output appeared — which is equally true of a tool that opened the
/// archive, read it, and then refused. Here the input is one the tool cannot read at all, so
/// its silence about it is what proves nothing looked.
#[test]
fn the_option_fault_wins_over_the_input_fault() {
    let directory = TempDir::new("pairing");

    let missing = directory.join("not-here.zip");
    let decoy = directory.join("decoy.zip");
    fs::write(&decoy, b"this is not an archive").expect("writes the decoy");

    for broken in [&missing, &decoy] {
        let (code, message) = refusal(&["--quality".as_ref(), "0".as_ref(), broken.as_ref()]);
        assert_eq!(code, USAGE, "{message}");
        assert!(
            message.contains("--quality") && message.contains("1..=100"),
            "the message must be about the option: {message}"
        );
        assert!(
            !message.contains(&broken.display().to_string()),
            "the input was looked at: {message}"
        );
        assert!(!default_output(broken).exists());

        // The control: the same input with an accepted option value is refused for being
        // unreadable, at the runtime code. Without this row the assertions above would also
        // pass for a build that refused every invocation at the parser.
        let (code, message) = refusal(&["--quality".as_ref(), "80".as_ref(), broken.as_ref()]);
        assert_eq!(code, RUNTIME, "{message}");
        assert!(
            message.contains(&broken.display().to_string()),
            "a legal value should have reached the input: {message}"
        );
    }
}

/// A value drawn from a fixed set is refused with the set listed, on every flag that has one.
///
/// One rule over three flags from three Changes, so the completion generator's unknown-shell
/// refusal is covered by a rule that already held twice rather than by a clause of its own.
#[test]
fn a_rejected_value_from_a_fixed_set_lists_the_set() {
    let directory = TempDir::new("fixed-sets");
    let input = valid_input(&directory);

    let (code, message) = refusal(&["--dct".as_ref(), "fast".as_ref(), input.as_ref()]);
    assert_eq!(code, USAGE, "{message}");
    for name in DctMethod::NAMES {
        assert!(
            message.contains(name),
            "`{name}` is missing from: {message}"
        );
    }

    let (code, message) = refusal(&["--resize-mode".as_ref(), "nearest".as_ref(), input.as_ref()]);
    assert_eq!(code, USAGE, "{message}");
    for name in Filter::NAMES {
        assert!(
            message.contains(name),
            "`{name}` is missing from: {message}"
        );
    }

    let (code, message) = refusal(&["--completions".as_ref(), "tcsh".as_ref()]);
    assert_eq!(code, USAGE, "{message}");
    for shell in ["bash", "zsh", "fish", "powershell"] {
        assert!(
            message.contains(shell),
            "`{shell}` is missing from: {message}"
        );
    }
}

/// A refusal names the flag the user typed and the values it accepts, not the type behind it.
///
/// `--jobs` is the sharp case: it is a `NonZeroUsize`, and std's message for zero speaks of a
/// non-zero integer type. The flag's own value parser is what keeps that vocabulary off the
/// command line, and the range it accepts is stated rather than left to `--help`.
#[test]
fn a_refusal_speaks_of_the_flag_rather_than_of_the_type_behind_it() {
    let directory = TempDir::new("flag-vocabulary");
    let input = valid_input(&directory);

    for value in ["0", "abc"] {
        let (code, message) = refusal(&["--jobs".as_ref(), value.as_ref(), input.as_ref()]);
        assert_eq!(code, USAGE, "{message}");
        assert!(message.contains("--jobs"), "{message}");
        assert!(
            message.contains("give 1 to"),
            "the accepted range is missing: {message}"
        );
        let lowered = message.to_lowercase();
        for vocabulary in ["nonzero", "non-zero", "usize", "integer"] {
            assert!(
                !lowered.contains(vocabulary),
                "`{vocabulary}` is the type's word, not the flag's: {message}"
            );
        }
    }
}

/// No refusal path deletes the input, whatever combination of flags reached it.
///
/// `--delete-org` is the only flag on this surface that destroys a file, so every refusal it
/// can be given is a path the input has to survive. Survival is asserted rather than the
/// message, because survival is what the user cares about and a late refusal is what would
/// break it.
#[test]
fn a_refused_run_leaves_the_input_where_it_was() {
    let directory = TempDir::new("delete-org-refusals");

    let input = valid_input(&directory);
    let taken = directory.join("taken.zip");
    fs::write(&taken, b"already here").expect("writes the obstacle");
    let nowhere = directory.join("nowhere").join("out.zip");
    let empty = without_pages(&directory);
    let undecodable = with_an_undecodable_page(&directory);
    let tree = directory.join("pages");
    write_tree(&tree, &[("001.jpg", page_bytes(320, 440))]);

    let refusals: Vec<Vec<&OsStr>> = vec![
        // A usage fault, refused before anything is opened.
        vec![
            "--delete-org".as_ref(),
            "--quality".as_ref(),
            "0".as_ref(),
            input.as_ref(),
        ],
        // A refusal decided from the paths alone.
        vec![
            "--delete-org".as_ref(),
            "-o".as_ref(),
            taken.as_ref(),
            input.as_ref(),
        ],
        vec![
            "--delete-org".as_ref(),
            "-o".as_ref(),
            nowhere.as_ref(),
            input.as_ref(),
        ],
        // A refusal the input's own contents decided, which is the latest one there is: the
        // run had read the archive by the time it failed.
        vec!["--delete-org".as_ref(), empty.as_ref()],
        vec!["--delete-org".as_ref(), undecodable.as_ref()],
        // The flag's own refusal.
        vec!["--delete-org".as_ref(), tree.as_ref()],
    ];
    for args in refusals {
        let (code, message) = refusal(&args);
        assert_ne!(code, 0, "{args:?} succeeded: {message}");
        for survivor in [&input, &empty, &undecodable, &tree] {
            assert!(survivor.exists(), "{args:?} removed {}", survivor.display());
        }
    }
}

/// The same, for the input a refusal would be most tempted to remove.
///
/// A symbolic link is refused *because* removing it would take the link and leave the archive
/// it points at, so this asserts both survive.
#[cfg(unix)]
#[test]
fn a_refused_run_removes_neither_a_link_nor_what_it_points_at() {
    let directory = TempDir::new("delete-org-link");
    let input = valid_input(&directory);
    let link = directory.join("link.zip");
    std::os::unix::fs::symlink(&input, &link).expect("creates the link");

    let (code, message) = refusal(&["--delete-org".as_ref(), link.as_ref()]);
    assert_eq!(code, RUNTIME, "{message}");
    assert!(
        fs::symlink_metadata(&link).is_ok(),
        "the link was removed: {message}"
    );
    assert!(input.exists(), "the archive the link points at was removed");
}
