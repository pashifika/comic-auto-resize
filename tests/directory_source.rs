//! Reading a plain directory as an ordered sequence of named pages.
//!
//! The input with no stored order, so the order is the reader's choice and most of this file
//! is about that choice. The Go implementation made none — `fs.WalkDir` sorts byte-lexically
//! and nothing re-sorted afterwards — and shipped `page1`, `page10`, `page2`: a book with its
//! pages out of sequence, silently.
//!
//! Everything else here is the shared entry contract, asserted against an input that is not
//! an archive at all.

mod support;

use std::path::Path;
use std::process::Command;

use comic_auto_resize::pipeline::{self, RunError, Settings};
use comic_auto_resize::sink::{InputKind, default_output};
use comic_auto_resize::source::{Entries, Entry, Naming, Source, SourceError};

use support::{TempDir, page_bytes, read_archive, write_tree};

/// The binary under test, built by Cargo for this integration test at the same profile.
const BINARY: &str = env!("CARGO_BIN_EXE_comic-auto-resize");

fn page(width: u32) -> Vec<u8> {
    page_bytes(width, 24)
}

/// Every page the source yields, in order.
fn read_all(root: &Path) -> Result<Vec<Entry>, SourceError> {
    let mut source = Source::open(root, Naming::Stored)?;
    let mut entries = Vec::new();
    while let Some(entry) = source.next_entry() {
        entries.push(entry?);
    }
    Ok(entries)
}

fn names(root: &Path) -> Vec<String> {
    read_all(root)
        .expect("reads")
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

fn error_text(root: &Path) -> String {
    read_all(root)
        .err()
        .map_or_else(|| "no error".to_owned(), |error| error.to_string())
}

/// A directory holding `files`, inside a scratch directory that outlives the body.
fn with_tree(label: &str, files: &[(&str, Vec<u8>)], body: impl FnOnce(&TempDir, &Path)) {
    let scratch = TempDir::new(label);
    let root = scratch.join("vol1");
    write_tree(&root, files);
    body(&scratch, &root);
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

// ---------------------------------------------------------------- the chosen order

/// Task 3.4, and the defect the Go implementation shipped. Byte-lexical order gives
/// `page1`, `page10`, `page2`; this reader compares the digit runs by value.
#[test]
fn a_directorys_pages_are_read_in_numeric_order() {
    let files = [
        ("page10.jpg", page(32)),
        ("page1.jpg", page(30)),
        ("page2.jpg", page(31)),
    ];
    with_tree("dir-order", &files, |_scratch, root| {
        assert_eq!(names(root), ["page1.jpg", "page2.jpg", "page10.jpg"]);
    });
}

/// Depth-first with the path prefix preserved, and a subdirectory placed among the files
/// beside it by the same key rather than as a separate group.
#[test]
fn subdirectories_sort_among_the_files_beside_them_and_are_walked_depth_first() {
    let files = [
        ("cover.jpg", page(30)),
        ("chapter2/page1.jpg", page(33)),
        ("chapter10/page1.jpg", page(34)),
        ("chapter1/page2.jpg", page(32)),
        ("chapter1/page1.jpg", page(31)),
    ];
    with_tree("dir-nested", &files, |_scratch, root| {
        assert_eq!(
            names(root),
            [
                "chapter1/page1.jpg",
                "chapter1/page2.jpg",
                "chapter2/page1.jpg",
                "chapter10/page1.jpg",
                "cover.jpg",
            ]
        );
    });
}

/// Task 3.4's second half. Nothing in the comparison consults a locale, so the order is a
/// property of the names rather than of the machine that read them — asserted through the
/// binary, because that is where a locale would reach it.
#[test]
fn the_chosen_order_does_not_depend_on_the_hosts_locale() {
    let files = [
        ("Page10.jpg", page_bytes(300, 400)),
        ("page1.jpg", page_bytes(301, 400)),
        ("Page2.jpg", page_bytes(302, 400)),
        ("äpfel3.jpg", page_bytes(303, 400)),
    ];

    let mut orders = Vec::new();
    for locale in ["C", "en_US.UTF-8", "ja_JP.UTF-8", "tr_TR.UTF-8"] {
        let scratch = TempDir::new("dir-locale");
        let root = scratch.join("vol1");
        write_tree(&root, &files);

        let status = Command::new(BINARY)
            .arg(&root)
            .env("LC_ALL", locale)
            .env("LANG", locale)
            .status()
            .expect("runs the binary");
        assert!(status.success(), "the run failed under {locale}");

        let output = default_output(&root, InputKind::Directory).expect("named");
        orders.push((
            locale,
            read_archive(&output)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
        ));
    }

    // Which order, not merely that the four agree: a wrong-but-stable order would satisfy
    // agreement alone. Upper case sorts before lower case because everything that is not a
    // digit compares byte-wise, and `äpfel3` last because its first byte is above ASCII.
    let (first_locale, first) = &orders[0];
    assert_eq!(
        first,
        &["Page2.jpg", "Page10.jpg", "page1.jpg", "äpfel3.jpg"],
        "the order itself changed under {first_locale}"
    );
    for (locale, order) in &orders[1..] {
        assert_eq!(
            order, first,
            "{locale} ordered the pages differently from {first_locale}"
        );
    }
}

// ---------------------------------------------------------------- names

/// Task 3.5. Go unified the two input kinds by rooting both at `.`; the directory's own name
/// belongs to the output file, never to an entry.
#[test]
fn entry_names_are_relative_to_the_input_directory() {
    let files = [("page1.jpg", page(30)), ("ch1/page2.jpg", page(31))];
    with_tree("dir-names", &files, |_scratch, root| {
        let names = names(root);
        assert_eq!(names, ["ch1/page2.jpg", "page1.jpg"]);
        for name in &names {
            assert!(
                !name.contains("vol1"),
                "{name} carries the directory's name"
            );
        }
    });
}

#[test]
fn the_stored_name_reaches_the_output_with_only_its_extension_rewritten() {
    let files = [("ch1/page01.jpeg", page(30))];
    with_tree("dir-rename", &files, |_scratch, root| {
        assert_eq!(names(root), ["ch1/page01.jpg"]);
    });
}

// ---------------------------------------------------------------- what is passed over

#[test]
fn a_dot_name_and_a_non_page_are_passed_over() {
    let files = [
        ("page1.jpg", page(30)),
        ("ComicInfo.xml", b"<ComicInfo/>".to_vec()),
        (".DS_Store", vec![0; 16]),
        (".hidden/page9.jpg", page(31)),
    ];
    with_tree("dir-skips", &files, |_scratch, root| {
        assert_eq!(names(root), ["page1.jpg"]);
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
    with_tree("dir-mismatch", &files, |_scratch, root| {
        assert!(
            error_text(root).contains("named as JPEG"),
            "{}",
            error_text(root)
        );
    });
}

/// Task 3.2's second half. A filesystem reaches the unsafe-name case as readily as a crafted
/// archive does: a directory literally named `C:` is legal here and absolute on Windows, and
/// a backslash is a legal byte in a unix file name and a separator on Windows.
#[cfg(unix)]
#[test]
fn a_name_a_filesystem_allows_and_an_archive_must_not_carry_is_refused() {
    with_tree(
        "dir-drive-letter",
        &[("C:/page1.jpg", page(30))],
        |_scratch, root| {
            assert!(
                error_text(root).contains("drive letter"),
                "{}",
                error_text(root)
            );
        },
    );

    with_tree(
        "dir-backslash",
        &[("pages\\..\\..\\evil.jpg", page(30))],
        |_scratch, root| {
            assert!(
                error_text(root).contains("escapes its own directory"),
                "{}",
                error_text(root)
            );
        },
    );
}

/// A symbolic link is not followed and is refused rather than passed over: the walk cannot
/// tell a link to a page from a link to a chapter of them without resolving it, and a page
/// that vanishes in silence is the failure this reader exists to prevent.
///
/// Unix only, because creating a link on Windows needs a privilege an ordinary test run does
/// not have. The rule is not platform-specific; the fixture is.
#[cfg(unix)]
#[test]
fn a_symbolic_link_that_escapes_the_input_is_refused() {
    let scratch = TempDir::new("dir-symlink");
    let outside = scratch.join("outside.jpg");
    std::fs::write(&outside, page(40)).expect("writes");

    let root = scratch.join("vol1");
    write_tree(&root, &[("page1.jpg", page(30))]);
    std::os::unix::fs::symlink(&outside, root.join("page2.jpg")).expect("links");

    let message = error_text(&root);
    assert!(message.contains("page2.jpg"), "{message}");
    assert!(message.contains("symbolic link"), "{message}");
}

/// The rule is broader than "a link that escapes", and the breadth is the point: not
/// following a link means the walk cannot tell a link to a page from a link to a chapter of
/// them, so a link *inside* the input is refused for the same reason one pointing outside is.
/// A user with a `latest -> ch3` convenience link gets a named error rather than a book that
/// is quietly missing a chapter.
#[cfg(unix)]
#[test]
fn a_symbolic_link_that_does_not_escape_is_refused_too() {
    let scratch = TempDir::new("dir-symlink-inside");
    let root = scratch.join("vol1");
    write_tree(&root, &[("ch1/page1.jpg", page(30))]);
    std::os::unix::fs::symlink(root.join("ch1"), root.join("latest")).expect("links");

    let message = error_text(&root);
    assert!(message.contains("latest"), "{message}");
    assert!(message.contains("symbolic link"), "{message}");
}

/// A fifo named like a page would block `File::open` until a writer appeared. `Source::open`
/// already refuses one as an *input*; a child of a directory input reaches the same open and
/// needs the same refusal.
///
/// Asserted at `Source::open` rather than only through a read, because the two checks produce
/// the same message: the walk's refusal is the one that matters, since it happens before the
/// output file is created, and only opening the source separates them.
#[cfg(unix)]
#[test]
fn something_that_is_not_a_regular_file_is_refused_before_it_is_opened() {
    let scratch = TempDir::new("dir-fifo");
    let root = scratch.join("vol1");
    write_tree(&root, &[("page1.jpg", page(30))]);

    let fifo = root.join("page2.jpg");
    let status = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("runs mkfifo");
    assert!(status.success(), "the fixture needs a fifo");

    let error = Source::open(&root, Naming::Stored)
        .expect_err("the listing must refuse it, before the output exists");
    let message = error.to_string();
    assert!(message.contains("page2.jpg"), "{message}");
    assert!(message.contains("not a regular file"), "{message}");
}

/// Nothing is unpacked to disk. The directory reader only ever opens a file for reading, so
/// the guard is that a run creates nothing beside the input — the same assertion the zip, rar
/// and 7z readers each carry, made here so the contract holds for the input that is not an
/// archive.
#[test]
fn reading_a_directory_writes_nothing_to_disk() {
    let files = [("page1.jpg", page(30)), ("ch1/page2.jpg", page(31))];
    with_tree("dir-no-temp", &files, |scratch, root| {
        let before = tree_listing(scratch.path());
        assert_eq!(names(root).len(), 2);
        assert_eq!(before, tree_listing(scratch.path()));
    });
}

/// Every path under `directory`, sorted, so a stray temporary anywhere in the tree shows.
fn tree_listing(directory: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).expect("lists") {
            let entry = entry.expect("an entry");
            found.push(entry.path().to_string_lossy().into_owned());
            if entry.file_type().expect("a type").is_dir() {
                pending.push(entry.path());
            }
        }
    }
    found.sort();
    found
}

/// The listing runs at open and each page is read later, so a tree the tool does not own can
/// change in between. The file type is re-checked immediately before the open, which narrows
/// the window to a syscall rather than the length of the run — a page swapped for a link
/// after listing is refused rather than read from outside the input.
#[cfg(unix)]
#[test]
fn a_page_swapped_for_a_link_after_listing_is_refused_rather_than_read() {
    let scratch = TempDir::new("dir-swap");
    let outside = scratch.join("outside.jpg");
    std::fs::write(&outside, page(40)).expect("writes");

    let root = scratch.join("vol1");
    write_tree(&root, &[("page1.jpg", page(30)), ("page2.jpg", page(31))]);

    // Listed as two regular files, then the second becomes a link before it is read.
    let mut source = Source::open(&root, Naming::Stored).expect("opens");
    let first = source.next_entry().expect("a page").expect("reads");
    assert_eq!(first.name, "page1.jpg");

    std::fs::remove_file(root.join("page2.jpg")).expect("removes");
    std::os::unix::fs::symlink(&outside, root.join("page2.jpg")).expect("links");

    let error = source
        .next_entry()
        .expect("a second item")
        .expect_err("the swapped page must be refused");
    assert!(matches!(error, SourceError::SymbolicLink { .. }), "{error}");
}

// ---------------------------------------------------------------- output naming

/// Task 3.6. `vol1` and `vol1.zip` do not collide, so nothing but the `_resize` suffix and
/// this rule stops the output landing inside its own input, where the next run would read it
/// as a page.
#[test]
fn the_output_is_named_after_the_directory_and_written_beside_it() {
    let files = [
        ("page1.jpg", page_bytes(400, 600)),
        ("page2.jpg", page_bytes(401, 600)),
    ];
    with_tree("dir-output", &files, |scratch, root| {
        let output = default_output(root, InputKind::Directory).expect("named");
        assert_eq!(output, scratch.join("vol1_resize.zip"));
        assert!(!output.starts_with(root), "the output is inside its input");

        let source = Source::open(root, Naming::Stored).expect("opens");
        assert_eq!(
            pipeline::run(source, &output, &settings())
                .expect("runs")
                .pages,
            2
        );
        assert!(output.exists());
        assert_eq!(read_archive(&output).len(), 2);
    });
}

// ---------------------------------------------------------------- refusals at open

/// Task 3.9. A directory with no pages is not an unrecognised format: it is an input that
/// yielded nothing, and the message has to say so.
#[test]
fn a_directory_holding_no_page_says_so_rather_than_naming_a_format() {
    with_tree(
        "dir-no-pages",
        &[("ComicInfo.xml", b"<ComicInfo/>".to_vec())],
        |scratch, root| {
            let output = scratch.join("out.zip");
            let source = Source::open(root, Naming::Stored).expect("a directory always opens");
            let error = pipeline::run(source, &output, &settings()).expect_err("no pages");
            assert!(matches!(error, RunError::Empty), "{error}");

            let message = error.to_string();
            assert!(message.contains("no pages to process"), "{message}");
            assert!(
                !message.contains("not an archive"),
                "a directory must not be told it is an unrecognised format: {message}"
            );
        },
    );
}

/// A directory is recognised as an input in its own right, without a signature probe — which
/// it has no bytes to answer.
#[test]
fn a_directory_is_accepted_without_being_probed() {
    with_tree("dir-kind", &[("page1.jpg", page(30))], |_scratch, root| {
        assert!(matches!(
            Source::open(root, Naming::Stored).expect("opens"),
            Source::Directory(_)
        ));
    });
}

/// The whole run, through the binary, so the surface and the reader are exercised together.
#[test]
fn a_directory_runs_end_to_end_through_the_binary() {
    let files = [
        ("page10.jpg", page_bytes(402, 600)),
        ("page1.jpg", page_bytes(400, 600)),
        ("page2.jpg", page_bytes(401, 600)),
    ];
    with_tree("dir-cli", &files, |scratch, root| {
        let output = Command::new(BINARY).arg(root).output().expect("runs");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let written = scratch.join("vol1_resize.zip");
        assert!(written.exists(), "no output beside the input directory");
        assert_eq!(
            read_archive(&written)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            ["page1.jpg", "page2.jpg", "page10.jpg"]
        );
    });
}
