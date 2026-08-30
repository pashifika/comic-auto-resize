//! The streaming pass, end to end.
//!
//! Ordering, entry counts, and the refusals are exercised through `pipeline::run`. The
//! acceptance criteria are exercised through the built binary, because "the tool writes
//! `<stem>_resize.zip`" is a property of the binary rather than of the library.

mod support;

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use comic_auto_resize::page::{DecodeSettings, EncodeSettings, Filter, PageErrorKind};
use comic_auto_resize::pipeline::{self, Capacities, RunError, Settings};
use comic_auto_resize::policy::AUTO_WIDTH;
use comic_auto_resize::sink::default_output;
use comic_auto_resize::source::{Source, SourceError};

use support::{
    Framing, TempDir, corrupt_scan, framed_archive, jpeg_size, page_bytes, read_archive,
    write_archive, write_pages,
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

/// A reader that records how many bytes have been drawn through it.
///
/// The pipeline's window is a claim about the reader's restraint, and the reader is where it
/// has to be observed: an index-addressable reader knows where every entry is before it has
/// read any of them, so nothing but the window stops it from reading them all.
struct Counting<R> {
    inner: R,
    read: Arc<AtomicU64>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.read.fetch_add(read as u64, Ordering::Relaxed);
        Ok(read)
    }
}

impl<R: Seek> Seek for Counting<R> {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(from)
    }
}

/// Runs the pipeline over an in-memory archive, writing to `output`.
fn run(input: &[u8], output: &Path, jobs: usize) -> Result<u32, RunError> {
    let source = Source::zip(std::io::Cursor::new(input.to_vec()))?;
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

/// The credit window bounds the reader, and bounds it whether or not the reader can address
/// entries by index.
///
/// Observable only while nothing is written: the *first* page fails, so no credit is ever
/// returned and the reader can have taken no more than the window's worth. A reader free to
/// read ahead would have drawn the whole archive through by the time the failure surfaced.
#[test]
fn an_index_addressable_reader_does_not_read_past_the_window() {
    let page = page_bytes(1520, 2150);
    let mut entries: Vec<_> = (0..20)
        .map(|index| (format!("page{index:02}.jpg"), page.clone()))
        .collect();
    entries[0].1 = corrupt_scan(&page, 0);
    let input = archive_bytes(&entries);

    let jobs = NonZeroUsize::new(1).expect("non-zero");
    let window = Capacities::for_jobs(jobs).credits as u64;
    let read = Arc::new(AtomicU64::new(0));
    let source = Source::zip(Counting {
        inner: std::io::Cursor::new(input.clone()),
        read: Arc::clone(&read),
    })
    .expect("the entry table reads");

    let directory = TempDir::new("window");
    let output = directory.join("out.zip");
    let error = pipeline::run(source, &output, &settings(jobs.get()))
        .expect_err("the first page ends the run");
    assert!(
        matches!(&error, RunError::Page(_)),
        "expected a page failure, so that nothing was ever written: {error}"
    );

    let read = read.load(Ordering::Relaxed);
    let per_entry = page.len() as u64;
    assert!(
        read > per_entry,
        "the reader never read a page, so the bound below is vacuous: {read} B"
    );
    assert!(
        read < per_entry * (window + 1),
        "the reader drew {read} B at about {per_entry} B an entry, past the {window}-entry \
         window; the whole archive is {} B",
        input.len()
    );
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

/// An unreadable entry table fails the run, and fails it before anything is written.
#[test]
fn the_binary_refuses_an_archive_whose_entry_table_is_truncated() {
    let directory = TempDir::new("bad-directory");
    let input = directory.join("in.zip");
    let entries = [
        ("page01.jpg", page_bytes(320, 440)),
        ("page02.jpg", page_bytes(320, 440)),
    ];
    // The last central-directory record is cut mid-header, while the end record still says
    // how long the directory should have been.
    let bytes = framed_archive(
        &entries,
        Framing {
            truncated_directory: 24,
            ..Framing::default()
        },
    );
    fs::write(&input, &bytes).expect("writes the fixture");

    let output = Command::new(BINARY)
        .arg(&input)
        .output()
        .expect("runs the binary");
    assert!(
        !output.status.success(),
        "a truncated entry table must fail the run"
    );
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(
        message.contains("in.zip"),
        "stderr must name the input: {message}"
    );

    let leftovers: Vec<_> = fs::read_dir(directory.path())
        .expect("reads the directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "in.zip")
        .collect();
    assert!(
        leftovers.is_empty(),
        "a refused archive left something behind: {leftovers:?}"
    );
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

/// An existing partial is refused rather than opened.
///
/// The partial's name is derived from the output's, so it is predictable. Without exclusive
/// creation, anyone able to write the output directory could pre-place it as a link and have
/// this process truncate and overwrite the link's target, then rename the link into place.
#[test]
fn an_existing_partial_file_is_refused_without_being_written() {
    let input = archive_bytes(&[("page.jpg".to_owned(), page_bytes(320, 440))]);

    let directory = TempDir::new("partial");
    let output = directory.join("out.zip");
    let partial = directory.join("out.zip.part");
    fs::write(&partial, b"planted").expect("plants the partial");

    let error = run(&input, &output, 2).expect_err("an existing partial is refused");
    assert!(
        matches!(&error, RunError::PartialExists { .. }),
        "expected a partial refusal, got {error}"
    );
    assert_eq!(
        fs::read(&partial).expect("reads the plant"),
        b"planted",
        "the planted file was written to"
    );
    assert!(!output.exists());
}

/// An archive with no pages produces no archive, rather than an empty one.
///
/// An empty output would report success and then make the next run fail with
/// "already exists".
#[test]
fn an_archive_with_no_pages_writes_nothing() {
    let input = archive_bytes(&[
        (
            "ComicInfo.xml".to_owned(),
            b"<?xml version=\"1.0\"?>".to_vec(),
        ),
        ("notes.txt".to_owned(), b"nothing to see".to_vec()),
    ]);

    let directory = TempDir::new("empty");
    let output = directory.join("out.zip");
    let error = run(&input, &output, 2).expect_err("no pages is not a successful run");

    assert!(
        matches!(&error, RunError::Empty),
        "expected an empty-run refusal, got {error}"
    );
    assert!(!output.exists(), "an empty archive was installed");
    // Not even the partial.
    let leftovers: Vec<_> = fs::read_dir(directory.path())
        .expect("reads the directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
}

/// Two stored names that collide once renamed are reported as a collision.
#[test]
fn two_entries_that_rename_onto_one_name_are_named() {
    // `p.jpeg` and `p.jpg` both become `p.jpg`.
    let input = archive_bytes(&[
        ("p.jpeg".to_owned(), page_bytes(320, 440)),
        ("p.jpg".to_owned(), page_bytes(320, 440)),
    ]);

    let directory = TempDir::new("collision");
    let output = directory.join("out.zip");
    let error = run(&input, &output, 1).expect_err("one output name cannot hold two entries");

    assert!(
        matches!(&error, RunError::NameCollision { name } if name == "p.jpg"),
        "expected a collision naming the output name, got {error}"
    );
    assert!(!output.exists());
}

/// A traversing or absolute entry name is refused rather than carried into the output.
#[test]
fn an_unsafe_entry_name_is_refused() {
    for stored in [
        "../escape.jpg",
        "pages/../../escape.jpg",
        "/absolute.jpg",
        "\\absolute.jpg",
        "C:\\windows\\escape.jpg",
        "pages\\..\\..\\escape.jpg",
        // Windows strips a component's trailing spaces and dots, so these name the parent
        // there while an exact comparison against `..` would let them through.
        "pages/.. /escape.jpg",
        "pages\\.. \\escape.jpg",
    ] {
        let input = archive_bytes(&[(stored.to_owned(), page_bytes(64, 96))]);
        let directory = TempDir::new("unsafe-name");
        let output = directory.join("out.zip");

        let error = run(&input, &output, 1).expect_err("an unsafe stored name must be refused");
        assert!(
            matches!(&error, RunError::Source(SourceError::UnsafeName { .. })),
            "{stored}: expected an unsafe-name refusal, got {error}"
        );
        assert!(!output.exists(), "{stored}: an output was written");
    }
}

/// The output archive really holds only the pages, read back with the `zip` crate rather
/// than through this crate's own reader — which skips non-image entries and so could not
/// observe one being written.
#[test]
fn the_output_holds_only_image_entries() {
    let mut entries: Vec<_> = (0..4)
        .map(|index| (format!("page{index}.jpg"), page_bytes(320, 440)))
        .collect();
    entries.push((
        "ComicInfo.xml".to_owned(),
        b"<?xml version=\"1.0\"?>".to_vec(),
    ));
    let input = archive_bytes(&entries);

    let directory = TempDir::new("only-images");
    let output = directory.join("out.zip");
    assert_eq!(run(&input, &output, 2).expect("runs"), 4);

    let file = fs::File::open(&output).expect("opens the output");
    let mut archive = zip::ZipArchive::new(file).expect("reads the central directory");
    let names: Vec<_> = (0..archive.len())
        .map(|index| archive.by_index(index).expect("entry").name().to_owned())
        .collect();
    assert_eq!(names, ["page0.jpg", "page1.jpg", "page2.jpg", "page3.jpg"]);
}

/// Every long option `--help` lists, without its `--`.
fn help_options() -> Vec<String> {
    let output = Command::new(BINARY)
        .arg("--help")
        .output()
        .expect("runs the binary");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    let mut found: Vec<String> = text
        .split_whitespace()
        .filter_map(|word| word.strip_prefix("--"))
        .map(|word| word.trim_end_matches([',', '.', '>']).to_owned())
        .filter(|word| !word.is_empty())
        .collect();
    found.sort();
    found.dedup();
    found
}

/// A valid input, so a refusal is attributable to the flag rather than to the archive.
fn valid_input(directory: &TempDir) -> std::path::PathBuf {
    let path = directory.join("in.zip");
    write_pages(&path, 1, 320, 440);
    path
}

/// The flags the Go implementation had and this build does not exist with.
///
/// A flag may exist and be unimplemented, or not exist; it must not exist and silently do
/// the wrong thing. This asserts the second half — that they are genuinely absent, not
/// accepted and ignored.
#[test]
fn a_flag_this_build_does_not_implement_is_an_unknown_argument() {
    let directory = TempDir::new("unknown-flags");
    let input = valid_input(&directory);

    for flag in [
        "--pwd",
        "--charset",
        "--delete-org",
        "--jobs",
        "-r",
        "--ratio",
        "--split",
        "-o",
        "--out",
        "--small-skip",
        "--optimizer",
        "--progressive",
    ] {
        let output = Command::new(BINARY)
            .arg(flag)
            .arg(&input)
            .output()
            .expect("runs the binary");
        assert!(
            !output.status.success(),
            "{flag} was accepted; it must not exist"
        );
        let message = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(
            message.contains("unexpected") || message.contains("unknown"),
            "{flag} did not fail as an unknown argument: {message}"
        );
        assert!(
            !default_output(&input).exists(),
            "{flag} produced an output archive"
        );
    }
}

/// `--help` lists exactly what exists, in both directions.
#[test]
fn help_lists_every_implemented_option_and_nothing_else() {
    // The four the change implements, plus what clap adds for free.
    let mut expected = vec![
        "auto-width".to_owned(),
        "dct".to_owned(),
        "help".to_owned(),
        "quality".to_owned(),
        "resize-mode".to_owned(),
        "version".to_owned(),
    ];
    expected.sort();

    assert_eq!(help_options(), expected);
}

/// `MIN_EDGE` and the resource budget are internal constants, not options.
///
/// A limit a user can raise is a limit that will be raised to force a bad page through, so
/// their absence from the surface is the requirement.
#[test]
fn no_option_sets_the_minimum_edge_or_a_budget() {
    let listed = help_options().join(" ");
    for forbidden in ["min-edge", "minimum", "budget", "max-pixels", "max-bytes"] {
        assert!(
            !listed.contains(forbidden),
            "`{forbidden}` appears on the command line: {listed}"
        );
    }
}

/// An out-of-range value is refused by the parser, before the input is opened.
#[test]
fn an_out_of_range_option_value_is_refused_before_any_work() {
    let directory = TempDir::new("bad-values");
    let input = valid_input(&directory);

    for args in [
        vec!["--quality", "0"],
        vec!["--quality", "101"],
        vec!["--auto-width", "0"],
        vec!["--auto-width", "65536"],
        vec!["--resize-mode", "nearest"],
        vec!["--dct", "fast"],
    ] {
        let output = Command::new(BINARY)
            .args(&args)
            .arg(&input)
            .output()
            .expect("runs the binary");
        assert!(
            !output.status.success(),
            "{args:?} was accepted; the value is out of range"
        );
        // The input was valid, so nothing may have been produced from it.
        assert!(
            !default_output(&input).exists(),
            "{args:?} produced an output archive"
        );
    }

    // And the accepted values really are accepted, so the test above is not passing because
    // every value is refused.
    for args in [
        vec!["--quality", "1"],
        vec!["--quality", "100"],
        vec!["--auto-width", "65535"],
        vec!["--resize-mode", "nearest-neighbor"],
        vec!["--dct", "islow"],
    ] {
        let scratch = TempDir::new("good-values");
        let good = valid_input(&scratch);
        let status = Command::new(BINARY)
            .args(&args)
            .arg(&good)
            .status()
            .expect("runs the binary");
        assert!(status.success(), "{args:?} was refused but is in range");
    }
}

/// Each accepted flag changes the output in a way attributable to it.
///
/// A flag that parses and then does nothing is the failure this guards against.
#[test]
fn every_accepted_flag_changes_the_output() {
    fn run_with(args: &[&str], label: &str) -> Vec<u8> {
        let directory = TempDir::new(label);
        let input = directory.join("in.zip");
        write_pages(&input, 1, 1520, 2150);
        let status = Command::new(BINARY)
            .args(args)
            .arg(&input)
            .status()
            .expect("runs the binary");
        assert!(status.success(), "{args:?} failed");
        read_archive(&default_output(&input))
            .into_iter()
            .next()
            .expect("one page")
            .1
    }

    let baseline = run_with(&[], "flag-baseline");

    // A different target width changes the dimensions.
    let narrower = run_with(&["--auto-width", "1000"], "flag-width");
    assert_eq!(jpeg_size(&baseline), Some((1280, 1811)));
    assert_eq!(jpeg_size(&narrower), Some((1000, 1414)));

    // The other three change the bytes at the same dimensions.
    for (args, label) in [
        (["-q", "50"], "flag-quality"),
        (["--dct", "islow"], "flag-dct"),
        (["--resize-mode", "nearest-neighbor"], "flag-filter"),
    ] {
        let changed = run_with(&args, label);
        assert_eq!(
            jpeg_size(&changed),
            Some((1280, 1811)),
            "{args:?} changed the geometry"
        );
        assert_ne!(changed, baseline, "{args:?} did not change the output");
    }
}
