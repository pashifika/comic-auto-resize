//! Reading a rar archive as an ordered sequence of named pages.
//!
//! The fixtures are built by `tests/fixtures/make-rar-fixtures.sh`, which needs RARLAB's
//! `rar` — the only program that writes a RAR archive, because `UnRAR`'s licence forbids
//! re-creating the compression algorithm. So these tests skip, loudly, when the fixtures are
//! absent. A test that silently does not run is worse than one that says why.

mod support;

use std::path::{Path, PathBuf};

use comic_auto_resize::source::{
    ArchiveFormat, Entry, MAX_ENTRY_BYTES, Source, SourceError, detect,
};

use support::{TempDir, write_pages};

/// The fixture directory, or `None` with a message naming how to build it.
fn fixtures() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = std::env::var_os("CAR_RAR_FIXTURES")
        .map_or_else(|| root.join("tools/rar-fixtures"), PathBuf::from);
    if dir.is_dir() {
        return Some(dir);
    }
    eprintln!(
        "SKIPPED: no rar fixtures at {}. Build them with \
         `tests/fixtures/make-rar-fixtures.sh`.",
        dir.display()
    );
    None
}

/// Runs `body` against a named fixture, or skips.
fn with_fixture(name: &str, body: impl FnOnce(&Path)) {
    let Some(dir) = fixtures() else { return };
    let path = dir.join(name);
    assert!(
        path.is_file(),
        "{} is missing; re-run tests/fixtures/make-rar-fixtures.sh",
        path.display()
    );
    body(&path);
}

/// Every page the source yields, in order.
fn read_all(path: &Path) -> Result<Vec<Entry>, SourceError> {
    let mut source = Source::open(path)?;
    let mut yielded = Vec::new();
    while let Some(entry) = comic_auto_resize::source::Entries::next_entry(&mut source) {
        yielded.push(entry?);
    }
    Ok(yielded)
}

fn names_and_indices(entries: &[Entry]) -> Vec<(u32, String)> {
    entries
        .iter()
        .map(|entry| (entry.index, entry.name.clone()))
        .collect()
}

// ---------------------------------------------------------------- ordering

/// Task 3.1. The fixture's stored order is deliberately not its alphabetical order, so
/// "stored order" is an assertion rather than a coincidence.
#[test]
fn a_rar_yields_every_page_in_stored_order_with_a_gapless_index() {
    with_fixture("stored-order.rar", |path| {
        let entries = read_all(path).expect("reads");
        assert_eq!(
            names_and_indices(&entries),
            vec![
                (0, "page02.jpg".to_owned()),
                (1, "page00.jpg".to_owned()),
                (2, "page03.jpg".to_owned()),
                (3, "page01.jpg".to_owned()),
            ]
        );
    });
}

/// Task 3.2. The only fixture that reaches the solid dictionary and the decompressor: both
/// real samples are non-solid and entirely stored, so nothing else here exercises this path.
#[test]
fn a_solid_rar_yields_every_page_in_stored_order() {
    with_fixture("solid-compressed.rar", |path| {
        let entries = read_all(path).expect("reads");
        assert_eq!(
            names_and_indices(&entries),
            vec![
                (0, "page00.jpg".to_owned()),
                (1, "page01.jpg".to_owned()),
                (2, "page02.jpg".to_owned()),
                (3, "page03.jpg".to_owned()),
            ]
        );
        // Each entry packs to seven bytes in this fixture, so identical output can only come
        // from the shared dictionary having been reconstructed.
        let first = &entries[0].bytes;
        for entry in &entries[1..] {
            assert_eq!(
                &entry.bytes, first,
                "a solid entry decoded to something other than the page it stores"
            );
        }
    });
}

// ---------------------------------------------------------------- refusals

/// Task 3.3. The wrong error is the thing being guarded against: `Mismatch` says "named as
/// JPEG but its leading bytes are not", which is nonsense for something that is not a page.
#[test]
fn a_directory_entry_is_passed_over_rather_than_called_a_mismatch() {
    with_fixture("directory-entry.rar", |path| {
        let entries = read_all(path).expect("a directory entry must not fail the read");
        assert_eq!(
            names_and_indices(&entries),
            vec![
                (0, "pages/page00.jpg".to_owned()),
                (1, "page01.jpg".to_owned()),
            ],
            "the directory must be absent from the yielded pages and leave no gap in the index"
        );
    });
}

/// Task 3.4. The fixture declares 68,157,440 bytes and packs to under 3 KB, so an
/// implementation that read before checking would be obvious.
#[test]
fn an_entry_declaring_more_than_the_limit_is_refused_before_it_is_read() {
    with_fixture("oversize-entry.rar", |path| {
        let error = read_all(path).expect_err("an over-large entry must be refused");
        match error {
            SourceError::TooLarge { name, limit } => {
                assert_eq!(name, "huge.jpg", "the error must name the entry");
                assert_eq!(limit, MAX_ENTRY_BYTES, "the error must name the limit");
            }
            other => panic!("expected TooLarge, got {other}"),
        }
    });
}

/// Task 3.4, the half that says *before its data is read*.
///
/// The fixture above cannot show it: with the recorded-size check removed the bytes still
/// stop at the limit, because the sink refuses them, so the same `TooLarge` comes back either
/// way. Correct behaviour, but it means that test alone does not pin *when* the refusal
/// happens — measured, by deleting the check and watching it still pass.
///
/// This fixture is the same archive with its data cut off. The headers survive, so the
/// recorded size is reached exactly as before, but there is nothing behind it: refusing on
/// the header gives `TooLarge`, and reading first gives a CRC error instead. Deterministic,
/// with no timing in it.
#[test]
fn an_over_large_entry_is_refused_without_its_data_being_read() {
    with_fixture("oversize-truncated.rar", |path| {
        let error = read_all(path).expect_err("an over-large entry must be refused");
        match error {
            SourceError::TooLarge { name, limit } => {
                assert_eq!(name, "huge.jpg");
                assert_eq!(limit, MAX_ENTRY_BYTES);
            }
            other => panic!(
                "expected TooLarge from the recorded size; got {other}, which means the data \
                 was read before the header was believed"
            ),
        }
    });
}

/// Task 3.5. Opening a middle volume is the realistic mistake, and it is the volume that
/// carries the split-before flag.
#[test]
fn an_entry_continued_from_another_volume_is_refused() {
    with_fixture("split-entry.part2.rar", |path| {
        let error = read_all(path).expect_err("a volume set must be refused");
        match error {
            SourceError::Split { name } => {
                assert_eq!(name, "page01.jpg", "the error must name the entry");
            }
            other => panic!("expected Split, got {other}"),
        }
        assert!(
            error_text(path).contains("multi-volume set"),
            "the message must say the input is one part of a set"
        );
    });
}

fn error_text(path: &Path) -> String {
    read_all(path)
        .err()
        .map_or_else(String::new, |error| error.to_string())
}

/// Task 3.6. `unrar` panics rather than erroring on a path holding an interior NUL, and a
/// panic in the reader thread costs the run its message.
#[test]
fn an_input_path_containing_a_nul_is_refused_rather_than_panicking() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    // Not reachable from the command line, but the library API is public.
    let path = PathBuf::from(OsString::from_vec(b"pages\0.rar".to_vec()));
    let error = Source::rar(&path).expect_err("a NUL-bearing path must be refused");
    assert!(
        matches!(error, SourceError::UnsafePath),
        "expected UnsafePath, got {error}"
    );
}

// ---------------------------------------------------------------- the signature probe

/// Task 3.7. `.cbz` and `.cbr` are conventions the tools writing them get mixed up, so the
/// extension decides nothing.
#[test]
fn a_rar_named_cbz_is_read_as_rar() {
    with_fixture("stored-order.rar", |path| {
        let directory = TempDir::new("rar-as-cbz");
        let disguised = directory.join("book.cbz");
        std::fs::copy(path, &disguised).expect("copies the fixture");

        let entries = read_all(&disguised).expect("reads as rar despite the name");
        assert_eq!(entries.len(), 4);
        assert!(matches!(
            Source::open(&disguised).expect("opens"),
            Source::Rar(_)
        ));
    });
}

#[test]
fn a_zip_named_cbr_is_read_as_zip() {
    let directory = TempDir::new("zip-as-cbr");
    let disguised = directory.join("book.cbr");
    write_pages(&disguised, 2, 320, 480);

    assert!(matches!(
        Source::open(&disguised).expect("opens"),
        Source::Zip(_)
    ));
    let entries = read_all(&disguised).expect("reads as zip despite the name");
    assert_eq!(entries.len(), 2);
}

#[test]
fn a_file_matching_no_signature_is_refused_naming_the_formats() {
    let directory = TempDir::new("not-an-archive");
    let path = directory.join("book.cbz");
    std::fs::write(&path, b"this is not an archive at all").expect("writes the decoy");

    let error = Source::open(&path).expect_err("an unknown format must be refused");
    match error {
        SourceError::NotAnArchive { ref formats } => {
            assert!(formats.contains("zip"), "must name zip: {formats}");
            assert!(formats.contains("rar"), "must name rar: {formats}");
        }
        other => panic!("expected NotAnArchive, got {other}"),
    }
}

/// Both real samples are covered by the probe: one is RAR 4.x and the other RAR 5.0, and the
/// RAR 5.0 signature extends the RAR 4.x one, so the order is what keeps this deterministic.
#[test]
fn both_rar_generations_probe_as_rar() {
    assert_eq!(detect(b"Rar!\x1a\x07\x00"), Some(ArchiveFormat::Rar));
    assert_eq!(detect(b"Rar!\x1a\x07\x01\x00"), Some(ArchiveFormat::Rar));
}

// ---------------------------------------------------------------- the shared contract

/// Task 3.8, the properties `archive-source` says hold for every format. Mirrors the shapes
/// the zip tests use, so a divergence between the two readers shows up as a divergence.
#[test]
fn an_entry_no_extension_claims_is_passed_over() {
    with_fixture("mixed-entries.rar", |path| {
        let entries = read_all(path).expect("reads");
        // The stored name, exactly, rather than an extension comparison: the fixture stores
        // `notes.xml` and the assertion is that this name is not among the pages.
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(
            !names.contains(&"notes.xml"),
            "a non-page entry reached the pipeline: {names:?}"
        );
    });
}

#[test]
fn the_stored_name_reaches_the_output_with_only_its_extension_rewritten() {
    with_fixture("mixed-entries.rar", |path| {
        let entries = read_all(path).expect("reads");
        assert_eq!(
            names_and_indices(&entries),
            vec![
                (0, "page00.jpg".to_owned()),
                // Stored as `.jpeg`, rewritten to the encoder's extension, everything before
                // it untouched — and the index has no gap where `notes.xml` was skipped.
                (1, "page01.jpg".to_owned()),
            ]
        );
    });
}

#[test]
fn an_entry_whose_extension_and_content_disagree_is_an_error_not_a_skip() {
    with_fixture("mismatch-entry.rar", |path| {
        let error = read_all(path).expect_err("a mismatched entry must fail the read");
        match error {
            SourceError::Mismatch { name, declared } => {
                assert_eq!(name, "page01.jpg");
                assert_eq!(declared, "JPEG");
            }
            other => panic!("expected Mismatch, got {other}"),
        }
    });
}

#[test]
fn a_traversing_stored_name_is_refused_rather_than_sanitised() {
    with_fixture("traversing-name.rar", |path| {
        let error = read_all(path).expect_err("a traversing name must be refused");
        match error {
            SourceError::UnsafeName { name, reason } => {
                assert_eq!(name, "../page00.jpg");
                assert_eq!(reason, "the name escapes its own directory");
            }
            other => panic!("expected UnsafeName, got {other}"),
        }
    });
}

#[test]
fn an_absolute_stored_name_is_refused() {
    with_fixture("absolute-name.rar", |path| {
        let error = read_all(path).expect_err("an absolute name must be refused");
        match error {
            SourceError::UnsafeName { name, reason } => {
                assert_eq!(name, "/abs/page00.jpg");
                assert_eq!(reason, "the name is absolute");
            }
            other => panic!("expected UnsafeName, got {other}"),
        }
    });
}

/// Nothing is unpacked to disk. `unrar` can extract to a file and this reader never asks it
/// to — it runs the DLL's test operation and captures the bytes through the data callback —
/// so the guard is that reading an archive creates nothing beside it.
#[test]
fn reading_a_rar_writes_nothing_to_disk() {
    with_fixture("stored-order.rar", |source| {
        let directory = TempDir::new("no-temp-file");
        let path = directory.join("book.rar");
        std::fs::copy(source, &path).expect("copies the fixture");

        let entries = read_all(&path).expect("reads");
        assert_eq!(entries.len(), 4);

        let left: Vec<_> = std::fs::read_dir(directory.path())
            .expect("reads the directory")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "book.rar")
            .collect();
        assert!(left.is_empty(), "reading left files behind: {left:?}");
    });
}
