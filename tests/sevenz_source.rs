//! Reading a 7z archive as an ordered sequence of named pages.
//!
//! Unlike rar, 7z has an open writer, so every fixture here is generated at test time and
//! nothing is committed. What is *not* unlike rar is the evidence problem: `samples/` holds
//! two zips and two rars and no 7z at all, so every claim in this file rests on an archive
//! `7zz` wrote rather than on one a real user produced. Recorded in the Change's evidence
//! directory as well, because it is the kind of gap that is invisible from a green run.
//!
//! The tests skip, loudly, when no 7-Zip archiver is on `PATH`.

mod support;

use std::path::Path;

use comic_auto_resize::pipeline::{self, RunError};
use comic_auto_resize::source::{
    Entries, Entry, MAX_ENTRY_BYTES, Naming, Source, SourceError, ZipSource,
};

use support::{TempDir, page_bytes, seven_zip, seven_zip_listing, write_seven_zip};

/// A small page, distinguishable from its neighbours by size.
fn page(width: u32) -> Vec<u8> {
    page_bytes(width, 24)
}

/// Runs `body` with a 7z archive holding `files`, or skips when there is no archiver.
fn with_archive(label: &str, files: &[(&str, Vec<u8>)], flags: &[&str], body: impl FnOnce(&Path)) {
    if seven_zip().is_none() {
        return;
    }
    let directory = TempDir::new(label);
    let archive = directory.join("fixture.7z");
    write_seven_zip(&archive, &directory.join("staging"), files, flags);
    body(&archive);
}

/// Every page the source yields, in order.
fn read_all(path: &Path) -> Result<Vec<Entry>, SourceError> {
    let mut source = Source::open(path, Naming::Stored)?;
    let mut entries = Vec::new();
    while let Some(entry) = source.next_entry() {
        entries.push(entry?);
    }
    Ok(entries)
}

fn names_and_indices(entries: &[Entry]) -> Vec<(u32, String)> {
    entries
        .iter()
        .map(|entry| (entry.index, entry.name.clone()))
        .collect()
}

fn error_text(path: &Path) -> String {
    read_all(path)
        .err()
        .map_or_else(|| "no error".to_owned(), |error| error.to_string())
}

// ---------------------------------------------------------------- the shared contract

/// Task 3.1. 7-Zip sorts by name when it writes, so `page1`, `page10`, `page2` *is* the
/// stored order — and it is deliberately not the order the pages should be read in if the
/// reader were re-sorting them. An archive's order is the archive's, and this reader does not
/// second-guess it; only a directory, which has no stored order, gets one chosen for it.
#[test]
fn a_7z_yields_every_page_in_stored_order_with_a_gapless_index() {
    let files = [
        ("page1.jpg", page(30)),
        ("page2.jpg", page(31)),
        ("page10.jpg", page(32)),
    ];
    with_archive("sevenz-order", &files, &[], |archive| {
        // Validated by an implementation that is not the one under test.
        assert_eq!(
            seven_zip_listing(archive),
            ["page1.jpg", "page10.jpg", "page2.jpg"],
            "the fixture does not have the stored order this test depends on"
        );

        let entries = read_all(archive).expect("reads");
        assert_eq!(
            names_and_indices(&entries),
            [
                (0, "page1.jpg".to_owned()),
                (1, "page10.jpg".to_owned()),
                (2, "page2.jpg".to_owned()),
            ]
        );
    });
}

/// A directory entry is passed over on its own mark, before the extension filter runs, so a
/// directory named like a page is not diagnosed as one whose bytes disagree with its name.
#[test]
fn a_directory_entry_is_passed_over_rather_than_called_a_mismatch() {
    let files = [
        ("pages/page1.jpg", page(30)),
        ("cover.jpg/page2.jpg", page(31)),
    ];
    with_archive("sevenz-directory", &files, &[], |archive| {
        let entries = read_all(archive).expect("reads");
        assert_eq!(
            names_and_indices(&entries),
            [
                (0, "cover.jpg/page2.jpg".to_owned()),
                (1, "pages/page1.jpg".to_owned()),
            ]
        );
    });
}

#[test]
fn an_entry_no_extension_claims_is_passed_over() {
    let files = [
        ("page1.jpg", page(30)),
        ("notes.xml", b"<ComicInfo/>".to_vec()),
        ("page2.jpg", page(31)),
    ];
    with_archive("sevenz-mixed", &files, &[], |archive| {
        let entries = read_all(archive).expect("reads");
        assert_eq!(
            names_and_indices(&entries),
            [(0, "page1.jpg".to_owned()), (1, "page2.jpg".to_owned())]
        );
    });
}

#[test]
fn the_stored_name_reaches_the_output_with_only_its_extension_rewritten() {
    let files = [("pages/page01.jpeg", page(30))];
    with_archive("sevenz-rename", &files, &[], |archive| {
        let entries = read_all(archive).expect("reads");
        assert_eq!(entries[0].name, "pages/page01.jpg");
    });
}

#[test]
fn an_entry_whose_extension_and_content_disagree_is_an_error_not_a_skip() {
    let files = [
        ("page1.jpg", page(30)),
        (
            "page2.jpg",
            b"this is not a JPEG, whatever the name says".to_vec(),
        ),
    ];
    with_archive("sevenz-mismatch", &files, &[], |archive| {
        assert!(
            error_text(archive).contains("named as JPEG"),
            "{}",
            error_text(archive)
        );
    });
}

/// `-spf` is the only way to make 7-Zip store a name it would otherwise strip.
///
/// The stored separator is the host's, not the format's: the same command writes
/// `../page1.jpg` on unix and `..\page1.jpg` on Windows. That is exactly why `unsafe_name`
/// treats both as separators, so the fixture is checked for either and the refusal is
/// asserted exactly.
#[test]
fn a_traversing_stored_name_is_refused_rather_than_sanitised() {
    if seven_zip().is_none() {
        return;
    }
    let directory = TempDir::new("sevenz-traversing");
    let staging = directory.join("staging");
    let inner = staging.join("in");
    support::write_tree(&inner, &[("keep", Vec::new())]);
    support::write_tree(&staging, &[("page1.jpg", page(30))]);

    let archive = directory.join("traversing.7z");
    let program = seven_zip().expect("checked");
    let status = std::process::Command::new(program)
        .args(["a", "-t7z", "-bso0", "-bsp0", "-spf"])
        .arg(&archive)
        .arg("../page1.jpg")
        .current_dir(&inner)
        .status()
        .expect("runs the archiver");
    assert!(status.success());

    let listing = seven_zip_listing(&archive);
    assert_eq!(
        listing
            .iter()
            .map(|name| name.replace('\\', "/"))
            .collect::<Vec<_>>(),
        ["../page1.jpg"],
        "the fixture does not store the traversing name this test depends on"
    );

    assert!(
        error_text(&archive).contains("escapes its own directory"),
        "{}",
        error_text(&archive)
    );
}

/// A drive letter in a *nested* component is a Windows drive-relative path, so pushing it
/// onto an extraction root discards the root. A check anchored at byte zero let it through.
#[test]
fn a_drive_letter_in_a_nested_component_is_refused() {
    if seven_zip().is_none() {
        return;
    }
    let directory = TempDir::new("sevenz-nested-drive");
    let staging = directory.join("staging");
    support::write_tree(&staging, &[("safe/C:page.jpg", page(30))]);

    let archive = directory.join("nested-drive.7z");
    write_seven_zip(&archive, &staging, &[("safe/C:page.jpg", page(30))], &[]);

    // 7-Zip stores the `safe` directory as its own entry; the page is the one that matters.
    assert!(
        seven_zip_listing(&archive)
            .iter()
            .any(|name| name.replace('\\', "/") == "safe/C:page.jpg"),
        "the fixture does not store the nested drive component this test depends on: {:?}",
        seven_zip_listing(&archive)
    );

    let message = error_text(&archive);
    assert!(message.contains("drive letter"), "{message}");
}

/// `--fix-idx` against 7z, the one format whose entry total is read off a field rather than a
/// method: `reader.archive().files.len()`.
#[test]
fn renumbering_takes_its_width_from_the_7z_entry_table() {
    let files = [
        ("cover.jpg", page(30)),
        ("ch1/page7.jpg", page(31)),
        ("ch1/page8.jpg", page(32)),
        ("ch2/page9.jpg", page(33)),
    ];
    with_archive("sevenz-renumber", &files, &[], |archive| {
        let mut source = Source::open(archive, Naming::ByPosition).expect("opens");
        let mut names = Vec::new();
        while let Some(entry) = source.next_entry() {
            names.push(entry.expect("reads").name);
        }
        // Five entries — four files and the `ch1`/`ch2` directory entries 7-Zip stores —
        // so one digit, positions in read order, and `ch2` restarting at one.
        assert_eq!(
            names,
            [
                "ch1/page_1.jpg",
                "ch1/page_2.jpg",
                "ch2/page_1.jpg",
                "cover.jpg",
            ]
        );
    });
}

/// Every item the source produces, errors included, so a reader that keeps going after it has
/// failed is visible rather than hidden behind an early return.
fn read_every(path: &Path) -> Vec<Result<Entry, SourceError>> {
    let mut source = Source::open(path, Naming::Stored).expect("opens");
    let mut items = Vec::new();
    while let Some(item) = source.next_entry() {
        items.push(item);
    }
    items
}

/// The callback's `Ok(false)` ends one *block*, not the walk: `ArchiveReader::for_each_entries`
/// discards the boolean its block decoder returns and starts the next block. So a reader that
/// stops that way carries on producing after it has already failed — and on a hostile archive
/// it decodes every remaining block into a sink first.
///
/// Two blocks, one entry each (`-ms=off`). Block 0 holds a mismatch, block 1 holds a good
/// page. A reader that really stops offers one item; one that only ended a block offers the
/// page as well, after the run was already over.
#[test]
fn an_error_stops_the_walk_across_blocks_not_only_within_one() {
    let files = [
        (
            "a_bad.jpg",
            b"this is not a JPEG, whatever the name says".to_vec(),
        ),
        ("b_page.jpg", page(31)),
    ];
    with_archive("sevenz-cross-block", &files, &["-ms=off"], |archive| {
        let items = read_every(archive);
        assert_eq!(
            items.len(),
            1,
            "the walk produced {} items after failing: {items:?}",
            items.len()
        );
        assert!(matches!(items[0], Err(SourceError::Mismatch { .. })));
    });
}

/// Nothing is unpacked to disk: the archive is decoded through a reader, never extracted.
#[test]
fn reading_a_7z_writes_nothing_to_disk() {
    let files = [("page1.jpg", page(30)), ("page2.jpg", page(31))];
    with_archive("sevenz-no-temp", &files, &[], |archive| {
        let directory = archive.parent().expect("a parent");
        let before = listing(directory);
        read_all(archive).expect("reads");
        assert_eq!(before, listing(directory));
    });
}

fn listing(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .expect("lists")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------- the entry-size bound

/// The fixture declares 68,157,442 bytes and packs to about ten kilobytes, so an
/// implementation that read before checking would be obvious. 7-Zip sorts, so `huge.jpg`
/// comes before `page1.jpg` and the refusal happens before any page is produced.
#[test]
fn an_entry_declaring_more_than_the_limit_is_refused_before_it_is_read() {
    let mut huge = vec![0xFF, 0xD8];
    huge.resize(usize::try_from(MAX_ENTRY_BYTES).expect("fits") + 1024, 0);
    let files = [("huge.jpg", huge), ("page1.jpg", page(30))];
    with_archive("sevenz-oversize", &files, &[], |archive| {
        let message = error_text(archive);
        assert!(message.contains("huge.jpg"), "{message}");
        assert!(message.contains("larger than the limit"), "{message}");
    });
}

// ---------------------------------------------------------------- the draining trap

/// Task 3.3, and the defect this reader was written around.
///
/// Six entries in one solid block, two of them passed over because no candidate extension
/// claims them. `BlockDecoder::for_each_entries` does not drain what the callback leaves, and
/// without the drain the entry *after* a skip receives the skipped entry's bytes and
/// iteration ends early.
///
/// The passed-over entries hold JPEG bytes under an extension no candidate claims, which is
/// what makes this the *silent* case rather than a lucky one: a shift hands `page3.jpg` a
/// structurally valid JPEG, so the magic-byte probe is satisfied and only comparing the bytes
/// can tell. Each page is a different width, so the wrong page is a different picture too.
#[test]
fn a_skipped_entry_in_a_solid_block_does_not_corrupt_the_next() {
    let files = [
        ("page1.jpg", page(30)),
        ("page2.thumb", page(40)),
        ("page3.jpg", page(31)),
        ("page4.jpg", page(32)),
        ("page5.thumb", page(41)),
        ("page6.jpg", page(33)),
    ];
    // `-ms=on` is the default, but the whole point of this fixture is one shared block.
    with_archive("sevenz-solid-skip", &files, &["-ms=on"], |archive| {
        let entries = read_all(archive).expect("reads");
        assert_eq!(
            names_and_indices(&entries),
            [
                (0, "page1.jpg".to_owned()),
                (1, "page3.jpg".to_owned()),
                (2, "page4.jpg".to_owned()),
                (3, "page6.jpg".to_owned()),
            ],
            "every page in the archive must still be visited"
        );

        // Each page's own bytes, not its predecessor's. The widths differ, so a shift by one
        // would show here even though every candidate is a structurally valid JPEG.
        let expected = [page(30), page(31), page(32), page(33)];
        for (entry, want) in entries.iter().zip(expected) {
            assert_eq!(
                entry.bytes, want,
                "{} got another entry's bytes",
                entry.name
            );
        }
    });
}

// ---------------------------------------------------------------- refusals at open

/// The headers are plain and the data is encrypted, and this build carries no AES: the
/// `aes256` default feature is off, so the archive is refused by a named error rather than
/// read as rubbish.
#[test]
fn an_encrypted_archive_is_refused_by_a_named_error() {
    let files = [("page1.jpg", page(30))];
    with_archive("sevenz-encrypted", &files, &["-pSecret1"], |archive| {
        let message = error_text(archive);
        assert!(
            message.contains("cannot read the archive"),
            "an encrypted archive must be refused, got: {message}"
        );
    });
}

// ---------------------------------------------------------------- the signature probe

/// `.cbz` and `.cb7` are conventions the tools writing them get mixed up, so the extension
/// decides nothing.
#[test]
fn a_7z_named_cbz_is_read_as_7z() {
    let files = [("page1.jpg", page(30))];
    with_archive("sevenz-disguised", &files, &[], |archive| {
        let disguised = archive.with_file_name("book.cbz");
        std::fs::rename(archive, &disguised).expect("renames");
        assert!(matches!(
            Source::open(&disguised, Naming::Stored).expect("opens"),
            Source::SevenZ(_)
        ));
        assert_eq!(read_all(&disguised).expect("reads").len(), 1);
    });
}

// ---------------------------------------------------------------- end to end

/// Through the whole pipeline, with an independent reader checking the result: the output is
/// a zip whose entries are the input's pages in the input's order.
#[test]
fn a_7z_runs_end_to_end_and_the_output_keeps_the_order() {
    let files = [
        ("page1.jpg", page_bytes(400, 600)),
        ("page2.jpg", page_bytes(401, 600)),
        ("page10.jpg", page_bytes(402, 600)),
    ];
    with_archive("sevenz-pipeline", &files, &[], |archive| {
        let output = archive.with_file_name("out.zip");
        let source = Source::open(archive, Naming::Stored).expect("opens");
        let report = pipeline::run(source, &output, &settings()).expect("runs");
        assert_eq!(report.pages, 3);

        let written: Vec<String> = support::read_archive(&output)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(written, ["page1.jpg", "page10.jpg", "page2.jpg"]);
    });
}

/// An archive holding no page this build can read is not an output worth writing.
#[test]
fn a_7z_with_no_page_reports_that_rather_than_writing_an_empty_archive() {
    let files = [("notes.xml", b"<ComicInfo/>".to_vec())];
    with_archive("sevenz-empty", &files, &[], |archive| {
        let output = archive.with_file_name("out.zip");
        let source = Source::open(archive, Naming::Stored).expect("opens");
        let error = pipeline::run(source, &output, &settings()).expect_err("no pages");
        assert!(matches!(error, RunError::Empty), "{error}");
        assert!(!output.exists());
    });
}

fn settings() -> pipeline::Settings {
    pipeline::Settings {
        jobs: std::num::NonZeroUsize::new(2).expect("non-zero"),
        target_width: comic_auto_resize::policy::AUTO_WIDTH,
        filter: comic_auto_resize::page::Filter::default(),
        decode: comic_auto_resize::page::DecodeSettings::default(),
        encode: comic_auto_resize::page::EncodeSettings::default(),
    }
}

/// The reader's own file is never left behind, and the `ZipSource` import above is what keeps
/// this file honest about which reader produced the output it checks.
#[test]
fn the_output_is_read_back_by_the_zip_reader_not_by_the_7z_one() {
    let files = [("page1.jpg", page_bytes(400, 600))];
    with_archive("sevenz-output-kind", &files, &[], |archive| {
        let output = archive.with_file_name("out.zip");
        let source = Source::open(archive, Naming::Stored).expect("opens");
        pipeline::run(source, &output, &settings()).expect("runs");

        let file = std::fs::File::open(&output).expect("opens the output");
        let mut zip = ZipSource::new(file, Naming::Stored).expect("the output is a zip");
        assert_eq!(
            zip.next_entry()
                .expect("one page")
                .expect("reads")
                .name
                .as_str(),
            "page1.jpg"
        );
    });
}
