//! The streaming pass, end to end.
//!
//! Ordering, entry counts, and the refusals are exercised through `pipeline::run`. The
//! acceptance criteria are exercised through the built binary, because "the tool writes
//! `<stem>_resize.zip`" is a property of the binary rather than of the library.

mod support;

use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::Command;

use comic_auto_resize::page::{DecodeSettings, EncodeSettings, Filter, PageErrorKind};
use comic_auto_resize::pipeline::{self, RunError, Settings};
use comic_auto_resize::policy::AUTO_WIDTH;
use comic_auto_resize::sink::default_output;
use comic_auto_resize::source::Source;

use support::{
    TempDir, corrupt_scan, jpeg_size, page_bytes, read_archive, write_archive, write_pages,
};

/// The binary under test, built by Cargo for this integration test at the same profile.
const BINARY: &str = env!("CARGO_BIN_EXE_comic-auto-resize");

fn settings(jobs: usize) -> Settings {
    Settings {
        jobs: NonZeroUsize::new(jobs).expect("non-zero"),
        target_width: AUTO_WIDTH,
        filter: Filter::default(),
        decode: DecodeSettings::default(),
        encode: EncodeSettings::default(),
    }
}

/// Runs the pipeline over an in-memory archive, writing to `output`.
fn run(input: &[u8], output: &Path, jobs: usize) -> Result<u32, RunError> {
    let source = Source::zip(std::io::Cursor::new(input.to_vec()));
    pipeline::run(source, output, &settings(jobs)).map(|report| report.pages)
}

fn archive_bytes(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let directory = TempDir::new("build");
    let path = directory.join("in.zip");
    write_archive(&path, entries);
    fs::read(&path).expect("reads back what was just written")
}

#[test]
fn output_order_is_identical_at_one_worker_and_at_many() {
    // Stored out of alphabetical order, so a writer that sorted by name would be caught.
    let entries: Vec<_> = ["c.jpg", "a.jpg", "b.jpg", "e.jpg", "d.jpg"]
        .iter()
        .map(|name| ((*name).to_owned(), page_bytes(1520, 2150)))
        .collect();
    let input = archive_bytes(&entries);

    let directory = TempDir::new("order");
    let mut orders = Vec::new();
    for jobs in [1usize, 2, 8] {
        let output = directory.join(&format!("out-{jobs}.zip"));
        assert_eq!(run(&input, &output, jobs).expect("runs"), 5);
        let names: Vec<_> = read_archive(&output)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        orders.push(names);
    }

    assert_eq!(orders[0], ["c.jpg", "a.jpg", "b.jpg", "e.jpg", "d.jpg"]);
    assert!(
        orders.windows(2).all(|pair| pair[0] == pair[1]),
        "worker count changed the output order: {orders:?}"
    );
}

#[test]
fn a_slow_first_page_still_lands_first() {
    // The first page is much larger than the rest, so it finishes last at any worker count
    // above one. Order must not follow completion.
    let mut entries = vec![("slow.jpg".to_owned(), page_bytes(2400, 3400))];
    for index in 0..8 {
        entries.push((format!("fast{index}.jpg"), page_bytes(320, 440)));
    }
    let input = archive_bytes(&entries);

    let directory = TempDir::new("slow-first");
    let output = directory.join("out.zip");
    assert_eq!(run(&input, &output, 8).expect("runs"), 9);

    let names: Vec<_> = read_archive(&output)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(names[0], "slow.jpg", "the slow page did not land first");
    assert_eq!(names.len(), 9);
}

#[test]
fn n_image_entries_produce_exactly_n_entries_each_name_once() {
    let mut entries: Vec<_> = (0..12)
        .map(|index| (format!("pages/page{index:02}.jpg"), page_bytes(320, 440)))
        .collect();
    // Non-image entries are not pages and must not appear in the output.
    entries.push((
        "ComicInfo.xml".to_owned(),
        b"<?xml version=\"1.0\"?>".to_vec(),
    ));
    let input = archive_bytes(&entries);

    let directory = TempDir::new("counts");
    let output = directory.join("out.zip");
    assert_eq!(run(&input, &output, 4).expect("runs"), 12);

    let mut names: Vec<_> = read_archive(&output)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(names.len(), 12);
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 12, "a name appeared twice");
}

#[test]
fn a_run_that_fails_partway_leaves_no_output_archive() {
    let mut entries: Vec<_> = (0..6)
        .map(|index| (format!("page{index}.jpg"), page_bytes(1520, 2150)))
        .collect();
    // Named as a page, holding one libjpeg would have to repair: a single flipped byte of
    // entropy-coded data. The run must stop rather than write a book with a fabricated page.
    //
    // Offset 0 is measured, not guessed. In a progressive file the early scan bytes are DC
    // coefficients and not every flip is recoverable-with-a-warning: offset 2 of this page
    // decodes cleanly, while 0, 1, and 3 onward report JWRN_HUFF_BAD_CODE.
    entries.insert(
        3,
        (
            "page-damaged.jpg".to_owned(),
            corrupt_scan(&page_bytes(1520, 2150), 0),
        ),
    );
    let input = archive_bytes(&entries);

    let directory = TempDir::new("failure");
    let output = directory.join("out.zip");
    let error = run(&input, &output, 4).expect_err("a repaired page ends the run");

    assert!(
        matches!(&error, RunError::Page(page) if matches!(page.kind, PageErrorKind::Repaired { .. })),
        "expected a repaired-page refusal, got {error}"
    );
    assert!(
        error.to_string().contains("page-damaged.jpg"),
        "the error must name the page: {error}"
    );
    assert!(!output.exists(), "a failed run left an archive behind");
    // Nor the partial file it was building.
    let leftovers: Vec<_> = fs::read_dir(directory.path())
        .expect("reads the directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "in.zip")
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
}

#[test]
fn an_existing_output_is_refused_without_modifying_it() {
    let input = archive_bytes(&[("page.jpg".to_owned(), page_bytes(320, 440))]);

    let directory = TempDir::new("exists");
    let output = directory.join("out.zip");
    fs::write(&output, b"not an archive").expect("writes the decoy");

    let error = run(&input, &output, 2).expect_err("an existing output is refused");
    assert!(
        matches!(&error, RunError::OutputExists { .. }),
        "expected an output refusal, got {error}"
    );
    assert_eq!(
        fs::read(&output).expect("reads the decoy"),
        b"not an archive",
        "the existing file was modified"
    );
}

#[test]
fn no_temporary_file_survives_a_successful_run() {
    let input = archive_bytes(&[
        ("a.jpg".to_owned(), page_bytes(1520, 2150)),
        ("b.jpg".to_owned(), page_bytes(320, 440)),
    ]);

    let directory = TempDir::new("no-temps");
    let output = directory.join("out.zip");
    assert_eq!(run(&input, &output, 2).expect("runs"), 2);

    let names: Vec<_> = fs::read_dir(directory.path())
        .expect("reads the directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        ["out.zip"],
        "the only file written must be the output archive"
    );
}

/// Acceptance 1, 2, and 3, through the binary.
#[test]
fn the_binary_writes_a_smaller_archive_of_normalised_pages() {
    let directory = TempDir::new("acceptance-wide");
    let input = directory.join("pages-wide.zip");
    write_pages(&input, 3, 1520, 2150);

    let status = Command::new(BINARY)
        .arg(&input)
        .status()
        .expect("runs the binary");
    assert!(status.success(), "the binary exited with {status}");

    // Acceptance 1: the default output path.
    let output = default_output(&input);
    assert_eq!(output, directory.join("pages-wide_resize.zip"));
    assert!(output.exists(), "no output archive was written");

    // Acceptance 2: byte-smaller than the input.
    let before = fs::metadata(&input).expect("input metadata").len();
    let after = fs::metadata(&output).expect("output metadata").len();
    assert!(
        after < before,
        "output is {after} bytes against an input of {before}"
    );

    // Acceptance 3: every entry 1280 wide at the derived height, in order, under the input
    // names.
    let entries = read_archive(&output);
    assert_eq!(entries.len(), 3);
    for (index, (name, bytes)) in entries.iter().enumerate() {
        assert_eq!(*name, format!("pages/page{:04}.jpg", index + 1));
        assert_eq!(
            jpeg_size(bytes),
            Some((1280, 1811)),
            "{name} is not 1280x1811"
        );
    }
}

/// Acceptance 4, through the binary.
#[test]
fn the_binary_re_encodes_a_narrow_page_at_its_own_size() {
    let directory = TempDir::new("acceptance-narrow");
    let input = directory.join("pages-narrow.zip");
    write_pages(&input, 2, 1000, 1400);

    let status = Command::new(BINARY)
        .arg(&input)
        .status()
        .expect("runs the binary");
    assert!(status.success(), "the binary exited with {status}");

    let output = default_output(&input);
    let source = read_archive(&input);
    let resized = read_archive(&output);
    assert_eq!(resized.len(), 2);

    for ((_, original), (name, produced)) in source.iter().zip(&resized) {
        // Dimensions unchanged.
        assert_eq!(
            jpeg_size(produced),
            Some((1000, 1400)),
            "{name} was resized"
        );
        // And still re-encoded rather than copied: pass-through skips the resize only.
        assert_ne!(
            original, produced,
            "{name} came through as the input bytes, so the encoder did not run"
        );
    }
}

#[test]
fn the_binary_refuses_a_missing_input_and_a_non_archive() {
    let directory = TempDir::new("bad-input");

    let missing = directory.join("nope.zip");
    let output = Command::new(BINARY)
        .arg(&missing)
        .output()
        .expect("runs the binary");
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(
        message.contains("nope.zip"),
        "stderr must name the path: {message}"
    );
    assert!(!default_output(&missing).exists());

    let not_zip = directory.join("plain.zip");
    fs::write(&not_zip, b"this is not an archive").expect("writes the decoy");
    let output = Command::new(BINARY)
        .arg(&not_zip)
        .output()
        .expect("runs the binary");
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(
        message.contains("not a zip archive"),
        "stderr must say why: {message}"
    );
    assert!(!default_output(&not_zip).exists());
}

/// Writes the fixtures for the peak-memory measurement.
///
/// A generator rather than a check, and the fixtures are tens of megabytes, so it is
/// ignored by default and writes only where it is told:
///
/// ```sh
/// CAR_FIXTURE_DIR=/tmp/car-memory \
///   cargo test --locked --release --test pipeline -- --ignored --nocapture
/// ```
///
/// Acceptance criterion 5 is a *ratio* between the two, so the measurement itself does not
/// have to be portable — but the sampling method has to be recorded beside the number, which
/// is why it is done deliberately rather than folded into the suite.
#[test]
#[ignore = "generates large fixtures for the manual peak-memory measurement"]
fn write_memory_fixtures() {
    let Ok(directory) = std::env::var("CAR_FIXTURE_DIR") else {
        panic!("set CAR_FIXTURE_DIR to the directory the fixtures should be written to");
    };
    let directory = Path::new(&directory);
    fs::create_dir_all(directory).expect("creates the fixture directory");

    // The same page size in both, so page count is the only variable.
    for pages in [100u32, 1000] {
        let path = directory.join(format!("pages-{pages}.zip"));
        write_pages(&path, pages, 1520, 2150);
        let size = fs::metadata(&path).expect("metadata").len();
        println!("{}: {pages} pages, {size} bytes", path.display());
    }
}
