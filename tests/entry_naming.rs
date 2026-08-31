//! `--fix-idx`: rewriting each page's name to carry its own position.
//!
//! The rule is one sentence — replace the stem's trailing digit run with the entry's place in
//! read order, at a width the entry total sets, restarting per directory — and this file is
//! the cases that sentence has to survive. Every one of them is a scenario in
//! `archive-source`'s delta, so a change to the rule shows up here rather than in a diff of
//! two archives.
//!
//! The fixtures are zips, built in memory, because the rule is a property of the shared name
//! helper rather than of any one reader. The two that are not — that a directory input gets
//! the same treatment, and that the flag reaches the reader from the command line — are
//! asserted through the real input kinds at the end.

mod support;

use std::io::Cursor;
use std::process::Command;

use comic_auto_resize::pipeline::{self, RunError, Settings};
use comic_auto_resize::sink::{InputKind, default_output};
use comic_auto_resize::source::{Entries, ReadOptions, Source, SourceError, ZipSource};

use support::{TempDir, by_position, page_bytes, read_archive, write_archive, write_tree};

/// A small page, distinguishable from its neighbours by width.
fn page(width: u32) -> Vec<u8> {
    page_bytes(width, 24)
}

/// A zip holding `names` in that stored order, each with its own page.
fn archive(names: &[&str]) -> Vec<u8> {
    let entries: Vec<(String, Vec<u8>)> = names
        .iter()
        .enumerate()
        .map(|(at, name)| {
            (
                (*name).to_owned(),
                page(30 + u32::try_from(at).expect("a small fixture")),
            )
        })
        .collect();
    let scratch = TempDir::new("naming-archive");
    let path = scratch.join("fixture.zip");
    write_archive(&path, &entries);
    std::fs::read(&path).expect("reads back the fixture")
}

/// Every output name the reader produces for `bytes` under `options`.
fn output_names(bytes: &[u8], options: &ReadOptions) -> Result<Vec<String>, SourceError> {
    let mut source = ZipSource::new(Cursor::new(bytes.to_vec()), options)?;
    let mut names = Vec::new();
    while let Some(entry) = source.next_entry() {
        names.push(entry?.name);
    }
    Ok(names)
}

fn renumbered(names: &[&str]) -> Vec<String> {
    output_names(&archive(names), &by_position()).expect("reads")
}

fn settings() -> Settings {
    Settings {
        jobs: std::num::NonZeroUsize::new(2).expect("non-zero"),
        target_width: comic_auto_resize::policy::AUTO_WIDTH,
        filter: comic_auto_resize::page::Filter::default(),
        decode: comic_auto_resize::page::DecodeSettings::default(),
        encode: comic_auto_resize::page::EncodeSettings::default(),
    }
}

// ---------------------------------------------------------------- the default

/// The rule Change 1 established stays true of the default path, which is why the flag exists
/// rather than the behaviour changing: a stored name reaches the output with only its
/// extension rewritten.
#[test]
fn renumbering_is_off_unless_it_is_asked_for() {
    let bytes = archive(&["page1.jpg", "page2.jpg", "page10.jpeg"]);
    assert_eq!(
        output_names(&bytes, &ReadOptions::default()).expect("reads"),
        ["page1.jpg", "page2.jpg", "page10.jpg"]
    );
}

// ---------------------------------------------------------------- the rule

/// The number written is the position, not the number the input recorded. Ninety-five entries
/// need two digits.
#[test]
fn a_page_is_renamed_to_its_own_position_at_the_width_the_total_requires() {
    let mut names: Vec<String> = (7..102).map(|number| format!("page{number}.jpg")).collect();
    names.sort();
    let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
    assert_eq!(borrowed.len(), 95);

    let renamed = renumbered(&borrowed);
    assert_eq!(&renamed[..3], ["page_01.jpg", "page_02.jpg", "page_03.jpg"]);
    assert_eq!(renamed.last().expect("95 entries"), "page_95.jpg");
}

#[test]
fn the_width_follows_the_entry_total_rather_than_a_fixed_guess() {
    let many: Vec<String> = (1..=210).map(|number| format!("p{number}.jpg")).collect();
    let borrowed: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(renumbered(&borrowed)[0], "p_001.jpg");

    // And a nine-page book is not padded to four digits.
    let few: Vec<String> = (1..=9).map(|number| format!("p{number}.jpg")).collect();
    let borrowed: Vec<&str> = few.iter().map(String::as_str).collect();
    assert_eq!(renumbered(&borrowed)[0], "p_1.jpg");
}

/// A rule that padded the digits already in the name would have to decide which of them are
/// the page number, and `1of10` is where every such rule fails. The positional rule never
/// asks the question.
#[test]
fn digits_that_are_not_a_page_number_are_not_interpreted() {
    assert_eq!(
        renumbered(&["page1of10.jpg", "page2of10.jpg"]),
        ["page1of_1.jpg", "page2of_2.jpg"]
    );
}

/// Not a case carved out of the rule: the operation replaces a trailing digit run and
/// `cover` has none, so nothing happens — and the counter does not advance, so the first
/// numbered page still takes position one.
#[test]
fn a_name_with_no_number_is_left_alone_and_consumes_no_position() {
    assert_eq!(
        renumbered(&["cover.jpg", "page1.jpg", "page2.jpg"]),
        ["cover.jpg", "page_1.jpg", "page_2.jpg"]
    );
}

#[test]
fn a_separator_is_not_doubled_and_a_bare_number_gains_none() {
    assert_eq!(renumbered(&["page_7.jpg"]), ["page_1.jpg"]);
    assert_eq!(renumbered(&["7.jpg"]), ["1.jpg"]);
}

/// Task 2.5a. The per-directory counter is claimed to work the same whatever the input's
/// kind, and the corpus had no archive that showed it — an archive stores `/` in an entry
/// name as readily as a filesystem does.
#[test]
fn each_directory_component_of_an_archives_entry_names_restarts_the_count() {
    let mut names: Vec<String> = (1..=9).map(|n| format!("ch1/page{n}.jpg")).collect();
    names.extend((1..=20).map(|n| format!("ch2/page{n}.jpg")));
    let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
    assert_eq!(borrowed.len(), 29);

    let renamed = renumbered(&borrowed);
    assert_eq!(renamed[0], "ch1/page_01.jpg");
    assert_eq!(renamed[8], "ch1/page_09.jpg");
    // `ch2` opens at one rather than at ten, and uses the width the 29-entry total requires.
    assert_eq!(renamed[9], "ch2/page_01.jpg");
    assert_eq!(renamed[28], "ch2/page_20.jpg");

    // The directory component itself is carried unchanged: this flag does not claim to know
    // how chapters are named.
    for name in &renamed {
        assert!(
            name.starts_with("ch1/") || name.starts_with("ch2/"),
            "{name} lost or changed its directory component"
        );
    }
}

/// Task 3.7a. A padding rule would have collapsed these two onto `page01.jpg` and leaned on
/// the writer's duplicate-name refusal to stay correct. The positional rule cannot produce
/// one name from two entries, so that refusal stays reachable only from a malformed input.
#[test]
fn two_entries_a_padding_rule_would_have_collapsed_get_distinct_names_and_no_refusal() {
    let names = renumbered(&["page1.jpg", "page_1.jpg"]);
    assert_eq!(names, ["page_1.jpg", "page_2.jpg"]);

    // And the writer agrees, which is the half an assertion about names cannot make.
    let scratch = TempDir::new("naming-collision");
    let input = scratch.join("book.zip");
    write_archive(
        &input,
        &[
            ("page1.jpg".to_owned(), page_bytes(400, 600)),
            ("page_1.jpg".to_owned(), page_bytes(401, 600)),
        ],
    );
    let output = scratch.join("out.zip");
    let source = Source::open(&input, &by_position()).expect("opens");
    let report = pipeline::run(source, &output, &settings()).expect("no duplicate-name refusal");
    assert_eq!(report.pages, 2);
    assert_eq!(
        read_archive(&output)
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        ["page_1.jpg", "page_2.jpg"]
    );
}

/// The claim stated as a property rather than as three examples: over a mixed input, every
/// output name is distinct and the writer is never asked to refuse one.
#[test]
fn no_input_shape_makes_two_entries_share_one_output_name() {
    let names = renumbered(&[
        "1.jpg",
        "cover.jpg",
        "ch1/1.jpg",
        "ch1/page1.jpg",
        "ch1/page_1.jpg",
        "page1.jpg",
        "page_1.jpg",
        "page1of10.jpg",
    ]);
    let distinct: std::collections::HashSet<&String> = names.iter().collect();
    assert_eq!(distinct.len(), names.len(), "{names:?}");
}

// ---------------------------------------------------------------- the other input kinds

#[test]
fn a_directory_input_is_renumbered_by_the_same_rule() {
    let scratch = TempDir::new("naming-directory");
    let root = scratch.join("vol1");
    write_tree(
        &root,
        &[
            ("cover.jpg", page(30)),
            ("page1.jpg", page(31)),
            ("page2.jpg", page(32)),
            ("page10.jpg", page(33)),
        ],
    );

    let mut source = Source::open(&root, &by_position()).expect("opens");
    let mut names = Vec::new();
    while let Some(entry) = source.next_entry() {
        names.push(entry.expect("reads").name);
    }
    // Read in numeric order, then numbered by that order.
    assert_eq!(
        names,
        ["cover.jpg", "page_1.jpg", "page_2.jpg", "page_3.jpg"]
    );
}

// ---------------------------------------------------------------- the surface

const BINARY: &str = env!("CARGO_BIN_EXE_comic-auto-resize");

/// Task 3.8. The width is derived from the entry total, so it is not the user's to set.
#[test]
fn the_flag_takes_no_value() {
    let scratch = TempDir::new("naming-surface");
    let input = scratch.join("book.zip");
    write_archive(&input, &[("page1.jpg".to_owned(), page_bytes(400, 600))]);

    let output = Command::new(BINARY)
        .arg("--fix-idx")
        .arg("4")
        .arg(&input)
        .output()
        .expect("runs the binary");
    assert!(
        !output.status.success(),
        "`--fix-idx 4` was accepted; the width is derived, not given"
    );
    assert!(
        !default_output(&input, InputKind::File)
            .expect("named")
            .exists(),
        "a rejected invocation produced an output archive"
    );
}

/// A user enabling this is accepting that every numbered page is renamed, so the help has to
/// say what the rule is rather than that there is one.
#[test]
fn the_flags_help_states_the_rule_in_full() {
    let output = Command::new(BINARY)
        .arg("--help")
        .output()
        .expect("runs the binary");
    let help = String::from_utf8_lossy(&output.stdout);
    let description = help
        .split("--fix-idx")
        .nth(1)
        .expect("--fix-idx is listed")
        .split("\n\n")
        .next()
        .expect("a description");

    for claim in ["position", "directory", "total"] {
        assert!(
            description.contains(claim),
            "the help does not say what `{claim}` means for this flag: {description}"
        );
    }
}

/// End to end, because the flag is only worth anything if it reaches the reader.
#[test]
fn the_flag_reaches_the_reader_and_the_default_run_is_unchanged() {
    let scratch = TempDir::new("naming-cli");
    let pages: Vec<(String, Vec<u8>)> = ["page1.jpg", "page2.jpg", "page10.jpg"]
        .iter()
        .enumerate()
        .map(|(at, name)| {
            (
                (*name).to_owned(),
                page_bytes(400 + u32::try_from(at).expect("small"), 600),
            )
        })
        .collect();

    let plain = scratch.join("plain.zip");
    write_archive(&plain, &pages);
    assert!(
        Command::new(BINARY)
            .arg(&plain)
            .status()
            .expect("runs")
            .success()
    );
    assert_eq!(
        read_archive(&default_output(&plain, InputKind::File).expect("named"))
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        ["page1.jpg", "page2.jpg", "page10.jpg"],
        "the default run must not renumber"
    );

    let fixed = scratch.join("fixed.zip");
    write_archive(&fixed, &pages);
    assert!(
        Command::new(BINARY)
            .arg("--fix-idx")
            .arg(&fixed)
            .status()
            .expect("runs")
            .success()
    );
    assert_eq!(
        read_archive(&default_output(&fixed, InputKind::File).expect("named"))
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        ["page_1.jpg", "page_2.jpg", "page_3.jpg"]
    );
}

/// Renumbering must not reach the writer's duplicate-name refusal, but the refusal itself
/// stays reachable from a malformed input — two stored names that become one once their
/// extensions are rewritten.
#[test]
fn the_writers_duplicate_name_refusal_is_still_reachable_without_renumbering() {
    let scratch = TempDir::new("naming-duplicate");
    let input = scratch.join("book.zip");
    write_archive(
        &input,
        &[
            ("cover.jpg".to_owned(), page_bytes(400, 600)),
            ("cover.jpeg".to_owned(), page_bytes(401, 600)),
        ],
    );
    let output = scratch.join("out.zip");
    let source = Source::open(&input, &ReadOptions::default()).expect("opens");
    let error = pipeline::run(source, &output, &settings()).expect_err("a collision");
    assert!(matches!(error, RunError::NameCollision { .. }), "{error}");
}
