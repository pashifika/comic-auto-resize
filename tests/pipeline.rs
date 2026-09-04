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
use comic_auto_resize::sink::InputKind;
use comic_auto_resize::source::{ReadOptions, SourceError, ZipSource};

use support::{
    Framing, TempDir, corrupt_scan, framed_archive, jpeg_size, page_bytes, read_archive,
    write_archive, write_pages,
};

/// The binary under test, built by Cargo for this integration test at the same profile.
const BINARY: &str = env!("CARGO_BIN_EXE_comic-auto-resize");

/// The output path for a file input, which every test here uses.
///
/// `sink::default_output` now takes the input's kind, because a directory has no extension to
/// remove; a file input can never be the unnamed case, so the `Result` is unwrapped here
/// rather than at every callsite.
fn default_output(input: &std::path::Path) -> std::path::PathBuf {
    comic_auto_resize::sink::default_output(input, InputKind::File)
        .expect("a file input always has a name")
}

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
    let source = ZipSource::new(
        std::io::Cursor::new(input.to_vec()),
        &ReadOptions::default(),
    )?;
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
    // `ZipSource` directly, which is the reason `pipeline::run` takes `Entries` rather than
    // `Source`: the enum names `File`, and a `File` cannot be instrumented.
    let source = ZipSource::new(
        Counting {
            inner: std::io::Cursor::new(input.clone()),
            read: Arc::clone(&read),
        },
        &ReadOptions::default(),
    )
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
    // The spec requires the refusal to name the path *and* the formats this build reads,
    // because "not a zip archive" stopped being the whole answer when rar arrived.
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
/// Acceptance criterion 5 is a *ratio* between the two zips, so the measurement itself does
/// not have to be portable — but the sampling method has to be recorded beside the number,
/// which is why it is done deliberately rather than folded into the suite.
///
/// The 7z and directory fixtures answer a different question. 7z's decoder allocates working
/// memory at the size the *archive* declares, so the same pages have to be measured through
/// it and through an input with no such term, and the large-dictionary archive is what makes
/// that term visible rather than inferred.
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

    // The same 1000 pages as a directory tree, which is also the staging area the 7z
    // fixtures are written from — so all three inputs hold byte-identical pages and the
    // only variable is the container.
    let tree = directory.join("pages-1000");
    let files: Vec<(String, Vec<u8>)> = (0..1000)
        .map(|page| (format!("page{page:04}.jpg"), page_bytes(1520, 2150)))
        .collect();
    let borrowed: Vec<(&str, Vec<u8>)> = files
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.clone()))
        .collect();
    support::write_tree(&tree, &borrowed);
    println!("{}: 1000 pages as a directory", tree.display());

    if support::seven_zip().is_none() {
        return;
    }
    for (name, flags) in [
        ("pages-1000.7z", Vec::new()),
        // Large enough that 7-Zip does not clamp the dictionary down to the input size.
        ("pages-1000-dict.7z", vec!["-m0=LZMA2:d128m"]),
    ] {
        let path = directory.join(name);
        let _ = fs::remove_file(&path);
        support::write_seven_zip(&path, &tree, &borrowed, &flags);
        let size = fs::metadata(&path).expect("metadata").len();
        println!(
            "{}: 1000 pages, {size} bytes, flags {flags:?}",
            path.display()
        );
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
///
/// `--charset` and `--pwd` were here until they were implemented, and moved into the list
/// below in the same Change — the movement `--fix-idx` made before them, and the one
/// `-o/--out` and `--delete-org` make now.
#[test]
fn a_flag_this_build_does_not_implement_is_an_unknown_argument() {
    let directory = TempDir::new("unknown-flags");
    let input = valid_input(&directory);

    for flag in [
        "--jobs",
        "-r",
        "--ratio",
        "--split",
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
    // The nine the tool implements, plus what clap adds for free.
    let mut expected = vec![
        "auto-width".to_owned(),
        "charset".to_owned(),
        "dct".to_owned(),
        "delete-org".to_owned(),
        "fix-idx".to_owned(),
        "help".to_owned(),
        "out".to_owned(),
        "pwd".to_owned(),
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

/// Each accepted flag that changes a page's *bytes* changes them in a way attributable to it.
///
/// A flag that parses and then does nothing is the failure this guards against. Nine flags
/// exist and four of them change page bytes, so the other five are asserted where their rules
/// live: `--fix-idx` in `tests/entry_naming.rs`, `--charset` and `--pwd` in
/// `tests/entry_charset.rs`, and `-o`/`--delete-org` — which change neither page bytes nor
/// entry names, only where the output goes and whether the input survives — in
/// `an_output_value_resolves_as_a_location_or_as_a_filename` and
/// `the_input_is_removed_only_after_the_output_is_in_place` below.
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

// ---------------------------------------------------------------- the output path

/// A value naming a location is joined with the default name; a value naming anything else
/// is the output path exactly.
///
/// The two arms are the boundary of what the tool takes responsibility for, so both are
/// asserted against the file that appears rather than against the message. A fresh input per
/// case, because a case that succeeds leaves an output behind.
#[test]
fn an_output_value_resolves_as_a_location_or_as_a_filename() {
    // A trailing separator selects the location arm, and it has to be read before the value
    // becomes a `Path`, which normalises it away.
    let separators = TempDir::new("out-location");
    let input = valid_input(&separators);
    let destination = separators.join("dest");
    fs::create_dir(&destination).expect("creates the destination");
    let mut trailing = destination.clone().into_os_string();
    trailing.push(std::path::MAIN_SEPARATOR_STR);
    let status = Command::new(BINARY)
        .arg("-o")
        .arg(&trailing)
        .arg(&input)
        .status()
        .expect("runs the binary");
    assert!(status.success(), "a trailing separator was refused");
    assert!(
        destination.join("in_resize.zip").exists(),
        "a location did not get the default name joined to it"
    );

    // An existing directory is a location without one.
    let existing = TempDir::new("out-existing-dir");
    let input = valid_input(&existing);
    let destination = existing.join("dest");
    fs::create_dir(&destination).expect("creates the destination");
    let status = Command::new(BINARY)
        .arg("-o")
        .arg(&destination)
        .arg(&input)
        .status()
        .expect("runs the binary");
    assert!(status.success(), "an existing directory was refused");
    assert!(
        destination.join("in_resize.zip").exists(),
        "an existing directory did not get the default name joined to it"
    );

    // The name joined to a location is the *default* name, so the input's extension is gone
    // rather than preserved or re-appended. This is the one regression the new resolution
    // code can cause: the reference tool writes `in.cbz` as `in_resize.cbz.zip`, and an
    // implementation that reached for `file_name` instead of `file_stem` would too.
    let carried = TempDir::new("out-location-stem");
    let input = carried.join("in.cbz");
    write_pages(&input, 1, 320, 440);
    let destination = carried.join("dest");
    fs::create_dir(&destination).expect("creates the destination");
    let status = Command::new(BINARY)
        .arg("-o")
        .arg(&destination)
        .arg(&input)
        .status()
        .expect("runs the binary");
    assert!(status.success(), "a `.cbz` input was refused");
    assert_eq!(
        fs::read_dir(&destination)
            .expect("reads")
            .map(|entry| entry.expect("an entry").file_name())
            .collect::<Vec<_>>(),
        vec![std::ffi::OsString::from("in_resize.zip")],
        "the location arm did not drop the input's extension"
    );

    // Anything else is a filename, verbatim: no extension appended, replaced or validated.
    // `out.cbz` is a zip archive called `out.cbz`, and `out` has no extension at all. The
    // bare `out.zip` is the commonest spelling and the one whose `Path::parent` is the empty
    // path, so it is the only case that exercises the "no directory component" arm of the
    // missing-directory check — run from the input's own directory to make it bare.
    for name in ["out.cbz", "out", "out.zip"] {
        let directory = TempDir::new("out-verbatim");
        let input = valid_input(&directory);
        let bare = name == "out.zip";
        let mut command = Command::new(BINARY);
        command.current_dir(directory.path()).arg("-o");
        if bare {
            command.arg(name).arg("in.zip");
        } else {
            command.arg(directory.join(name)).arg(&input);
        }
        let output = command.output().expect("runs the binary");
        assert!(
            output.status.success(),
            "-o {name} was refused: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let requested = directory.join(name);
        assert_eq!(
            read_archive(&requested).len(),
            1,
            "-o {name} is not a one-page zip"
        );
        assert!(
            !directory.join(&format!("{name}.zip")).exists(),
            "-o {name} had `.zip` appended, which is the reference tool's behaviour"
        );
        assert!(
            !default_output(&input).exists(),
            "-o {name} wrote the default name as well"
        );
    }
}

/// The directory to write into must already exist, through either arm, and the refusal names
/// the directory rather than the value.
///
/// Creating it is declined, and the containment check needs a path that canonicalises, which
/// a directory that is not there has none of.
#[test]
fn a_missing_output_directory_is_refused_and_named() {
    for (label, file) in [("missing-filename", "out.zip"), ("missing-location", "")] {
        let directory = TempDir::new(label);
        let input = valid_input(&directory);
        // `nowhere/out.zip` for the filename arm, and `nowhere/` for the location arm.
        let mut requested = directory.join("nowhere").into_os_string();
        requested.push(std::path::MAIN_SEPARATOR_STR);
        requested.push(file);
        let output = Command::new(BINARY)
            .arg("-o")
            .arg(&requested)
            .arg(&input)
            .output()
            .expect("runs the binary");
        assert!(!output.status.success(), "{label} was accepted");
        let message = String::from_utf8_lossy(&output.stderr);
        assert!(
            message.contains("nowhere") && message.contains("no such directory"),
            "{label} did not name the missing directory: {message}"
        );
        assert!(
            !directory.join("nowhere").exists(),
            "{label} created the directory"
        );
        assert!(
            !default_output(&input).exists(),
            "{label} fell back to the default name"
        );
    }

    // An empty value is neither arm: it names no directory to join a default name to and no
    // file to use verbatim, so it is refused rather than resolved to something unprintable.
    let directory = TempDir::new("empty-out");
    let input = valid_input(&directory);
    let output = Command::new(BINARY)
        .arg("-o")
        .arg("")
        .arg(&input)
        .output()
        .expect("runs the binary");
    assert!(!output.status.success(), "an empty -o was accepted");
    assert!(
        !default_output(&input).exists(),
        "an empty -o wrote an output"
    );
}

/// The existing-path refusal follows the resolved path, not the default one.
///
/// Go tests the raw `-o` value in its flag parser, so `-o out` passes its check and then
/// truncates an existing `out.zip`. Here the path that gets written is the path that is
/// checked — and the check runs before a thread starts, so the file it refused is untouched.
#[test]
fn the_existing_path_refusal_follows_the_resolved_path() {
    // The filename arm, against a file the default name would never have collided with.
    let directory = TempDir::new("resolved-exists");
    let input = valid_input(&directory);
    let taken = directory.join("out");
    fs::write(&taken, b"not an archive").expect("writes the obstacle");
    let output = Command::new(BINARY)
        .arg("-o")
        .arg(&taken)
        .arg(&input)
        .output()
        .expect("runs the binary");
    assert!(!output.status.success(), "an existing -o path was accepted");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already exists"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(&taken).expect("reads"),
        b"not an archive",
        "the existing file was disturbed"
    );

    // The location arm resolves to the default name inside the destination, so that is the
    // path the refusal is about.
    let destination = directory.join("dest");
    fs::create_dir(&destination).expect("creates the destination");
    let occupied = destination.join("in_resize.zip");
    fs::write(&occupied, b"already here").expect("writes the obstacle");
    let output = Command::new(BINARY)
        .arg("-o")
        .arg(&destination)
        .arg(&input)
        .output()
        .expect("runs the binary");
    assert!(!output.status.success());
    assert_eq!(fs::read(&occupied).expect("reads"), b"already here");
}

/// The input is removed once the output is in place, and not before.
#[test]
fn the_input_is_removed_only_after_the_output_is_in_place() {
    let directory = TempDir::new("delete-org");
    let input = valid_input(&directory);
    let output = Command::new(BINARY)
        .arg("--delete-org")
        .arg(&input)
        .output()
        .expect("runs the binary");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(default_output(&input).exists(), "no output was written");
    assert!(!input.exists(), "the input survived --delete-org");

    // One line, and the removal named on it.
    let line = String::from_utf8_lossy(&output.stdout);
    assert_eq!(line.lines().count(), 1, "still one line: {line}");
    assert!(
        line.contains("removed"),
        "the removal is not reported: {line}"
    );

    // And the other direction, which is the half that makes "exactly when" a claim: a run
    // without the flag prints what it always printed. The compositing note set this rule and
    // is asserted both ways in `tests/image_pages.rs`; this clause follows it.
    let kept = TempDir::new("delete-org-absent");
    let input = valid_input(&kept);
    let output = Command::new(BINARY)
        .arg(&input)
        .output()
        .expect("runs the binary");
    assert!(output.status.success());
    let line = String::from_utf8_lossy(&output.stdout);
    assert_eq!(line.lines().count(), 1, "still one line: {line}");
    assert!(
        !line.contains("removed"),
        "a run without the flag reported a removal: {line}"
    );
    assert!(input.exists(), "the input went without the flag");

    // A run that fails after the source was opened leaves the input alone. An archive
    // holding no page this build can read reaches that state: the source opened, the entries
    // were walked, and there was nothing to write.
    let directory = TempDir::new("delete-org-failed");
    let empty = directory.join("empty.zip");
    write_archive(
        &empty,
        &[("ComicInfo.xml".to_owned(), b"<ComicInfo/>".to_vec())],
    );
    let output = Command::new(BINARY)
        .arg("--delete-org")
        .arg(&empty)
        .output()
        .expect("runs the binary");
    assert!(
        !output.status.success(),
        "a page-less archive reported success"
    );
    assert!(empty.exists(), "a failed run removed the input");
}

/// An output resolving onto the input is refused by *existence*, not by an equality test.
///
/// The input necessarily exists by the time the output is resolved, because `Source::open`
/// succeeded first, so `path.exists()` catches every spelling of it. Two spellings equality
/// would miss are asserted here: a relative path, and — where the filesystem is
/// case-insensitive, which both release targets default to — a case variant.
#[test]
fn an_output_equal_to_the_input_is_refused_however_it_is_spelled() {
    let directory = TempDir::new("self-destruct");
    let input = valid_input(&directory);
    let name = input
        .file_name()
        .expect("a name")
        .to_str()
        .expect("ASCII")
        .to_owned();

    let mut spellings = vec![format!(".{}{name}", std::path::MAIN_SEPARATOR)];
    // Probed rather than assumed: a case-sensitive volume exists on both platforms, and the
    // claim is about the filesystem the run is on.
    let probe = directory.join("Probe");
    fs::write(&probe, b"").expect("writes the probe");
    if directory.join("probe").exists() {
        spellings.push(name.to_uppercase());
    }
    fs::remove_file(&probe).expect("removes the probe");

    for spelling in spellings {
        let output = Command::new(BINARY)
            .current_dir(directory.path())
            .arg("--delete-org")
            .arg("-o")
            .arg(&spelling)
            .arg(&name)
            .output()
            .expect("runs the binary");
        assert!(!output.status.success(), "-o {spelling} was accepted");
        let message = String::from_utf8_lossy(&output.stderr);
        assert!(
            message.contains("already exists"),
            "-o {spelling} was refused for the wrong reason: {message}"
        );
        assert!(input.exists(), "-o {spelling} destroyed the input");
    }
}

/// A dangling symbolic link at the resolved path is an entry, so the output is refused.
///
/// `Path::exists` follows the final link and answers false for a broken one, which would have
/// let the rename replace the link. The requirement is that the resolved path must not already
/// exist, and a broken link exists.
#[cfg(unix)]
#[test]
fn a_dangling_link_at_the_resolved_path_is_refused_rather_than_replaced() {
    let directory = TempDir::new("dangling-out");
    let input = valid_input(&directory);
    let requested = directory.join("out.zip");
    std::os::unix::fs::symlink(directory.join("nothing-here"), &requested).expect("links");

    let output = Command::new(BINARY)
        .arg("-o")
        .arg(&requested)
        .arg(&input)
        .output()
        .expect("runs the binary");
    assert!(!output.status.success(), "a dangling link was written over");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already exists"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::symlink_metadata(&requested)
            .expect("the link survives")
            .file_type()
            .is_symlink(),
        "the link was replaced"
    );
}

/// `--delete-org` is refused for a symbolic-link input, before any page is read.
///
/// `Source::open` follows the link and reads the archive it points at, while
/// `fs::remove_file` would unlink the link and leave that archive in place — the flag
/// reporting that it removed the input archive while the archive is still there. The bare run
/// is asserted too, so the refusal is attributable to the flag rather than to the link.
#[cfg(unix)]
#[test]
fn deleting_the_original_is_refused_for_a_symbolic_link_input() {
    let directory = TempDir::new("delete-org-link");
    let input = valid_input(&directory);
    let link = directory.join("link.zip");
    std::os::unix::fs::symlink(&input, &link).expect("links");

    let output = Command::new(BINARY)
        .arg("--delete-org")
        .arg(&link)
        .output()
        .expect("runs the binary");
    assert!(!output.status.success(), "--delete-org followed a link");
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(
        message.contains("symbolic link"),
        "the refusal does not say why: {message}"
    );
    assert!(input.exists(), "the archive the link pointed at is gone");
    assert!(
        fs::symlink_metadata(&link).is_ok(),
        "the link itself was removed"
    );
    assert!(
        !default_output(&link).exists(),
        "the refusal came after the output was written"
    );

    // Without the flag the link is an ordinary input and the run succeeds.
    let status = Command::new(BINARY)
        .arg(&link)
        .status()
        .expect("runs the binary");
    assert!(status.success(), "a link is a valid input without the flag");
    assert!(default_output(&link).exists());
}

/// A removal that fails names both the output that was written and the input that survived.
///
/// Without both facts the obvious retry meets the existing-output refusal and reports a
/// second, unrelated failure. The output goes outside the input's directory, because the
/// directory is what is made unwritable to deny the removal.
///
/// `written, but the input` is asserted rather than just the two paths, because the sibling
/// variant `OutputNotDurable` also formats both of them; without a discriminating substring
/// this would pass for either, and neither would be pinned.
#[test]
fn a_removal_that_fails_names_the_output_and_the_surviving_input() {
    let directory = TempDir::new("delete-org-denied");
    let held = directory.join("held");
    fs::create_dir(&held).expect("creates the input's directory");
    let input = held.join("in.zip");
    write_pages(&input, 1, 320, 440);
    let requested = directory.join("out.zip");

    // Two ways to deny an unlink, because the platforms do not share one. On unix the
    // permission that matters is the *parent's* write bit, and a read-only file unlinks
    // fine; on Windows it is the file's own read-only attribute, and `DeleteFileW` fails
    // with access denied. Either way the archive stays readable, so the run reaches the
    // removal rather than failing earlier.
    //
    // Both arms restore a snapshot taken before the change rather than clearing the bit they
    // set: `set_readonly(false)` is world-writable on unix and `clippy::all` refuses it even
    // inside a `#[cfg(windows)]` block, and restoring what was there is the more honest
    // teardown anyway — on Windows a file left read-only cannot be removed with the `TempDir`.
    let restore: Box<dyn FnOnce()> = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let original = fs::metadata(&held).expect("reads").permissions();
            fs::set_permissions(&held, PermissionsExt::from_mode(0o500))
                .expect("locks the directory");
            let held = held.clone();
            Box::new(move || {
                fs::set_permissions(&held, original).expect("restores the directory");
            })
        }
        #[cfg(windows)]
        {
            let original = fs::metadata(&input).expect("reads").permissions();
            let mut locked = original.clone();
            locked.set_readonly(true);
            fs::set_permissions(&input, locked).expect("locks the input");
            let input = input.clone();
            Box::new(move || {
                fs::set_permissions(&input, original).expect("restores the input");
            })
        }
    };
    let output = Command::new(BINARY)
        .arg("--delete-org")
        .arg("-o")
        .arg(&requested)
        .arg(&input)
        .output()
        .expect("runs the binary");
    restore();

    assert!(
        !output.status.success(),
        "a failed removal reported success"
    );
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(
        message.contains("written, but the input")
            && message.contains("out.zip")
            && message.contains("in.zip"),
        "the refusal does not name both paths as a failed removal: {message}"
    );
    assert!(requested.exists(), "the output was not written");
    assert!(input.exists(), "the input is gone after a failed removal");
}

/// A flush that fails abandons the removal, and the input survives.
///
/// This is the guard between a power loss and the total loss of the book, so it is asserted
/// rather than reasoned about. `fsync` on a healthy file cannot be made to fail, but the open
/// in front of it can: the archive is created with mode `0o666` before the umask, so a run
/// under `umask 0777` produces a mode `0o000` file that its own creator cannot reopen. The
/// umask is set by the shell rather than by this process, because it is per-process state and
/// the test harness is threaded.
#[cfg(unix)]
#[test]
fn a_flush_that_fails_abandons_the_removal() {
    let directory = TempDir::new("delete-org-undurable");
    let input = valid_input(&directory);
    let requested = directory.join("out.zip");
    let script = format!(
        "umask 0777; exec {} --delete-org -o {} {}",
        BINARY,
        requested.display(),
        input.display()
    );
    let output = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .output()
        .expect("runs the binary through a shell");

    assert!(
        !output.status.success(),
        "an unflushable output reported success"
    );
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(
        message.contains("could not be flushed to disk")
            && message.contains("out.zip")
            && message.contains("in.zip"),
        "the refusal does not name the flush and both paths: {message}"
    );
    assert!(
        input.exists(),
        "the input was removed although the output was never flushed"
    );
    assert!(requested.exists(), "the output was not written at all");
}

/// An input whose own entry cannot be queried is not deleted.
///
/// The query that fails is the one distinguishing a symbolic link from the archive it points
/// at, so treating its failure as "an ordinary file" would put the flag back where it started.
/// A parent without the traverse bit is what denies it: `lstat` of a known child needs `x` on
/// the directory, and the same run without the flag is asserted to show the refusal is the
/// flag's rather than the permission's.
#[cfg(unix)]
#[test]
fn an_input_whose_kind_cannot_be_established_is_not_deleted() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TempDir::new("delete-org-opaque");
    let held = directory.join("held");
    fs::create_dir(&held).expect("creates the input's directory");
    let input = held.join("in.zip");
    write_pages(&input, 1, 320, 440);

    let original = fs::metadata(&held).expect("reads").permissions();
    fs::set_permissions(&held, PermissionsExt::from_mode(0o644)).expect("drops the traverse bit");
    let refused = Command::new(BINARY)
        .arg("--delete-org")
        .arg(&input)
        .output()
        .expect("runs the binary");
    let without = Command::new(BINARY)
        .arg(&input)
        .output()
        .expect("runs the binary");
    fs::set_permissions(&held, original).expect("restores the directory");

    assert!(!refused.status.success());
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(
        message.contains("cannot be identified"),
        "the refusal is not the flag's: {message}"
    );
    assert!(input.exists(), "an unidentifiable input was removed");

    // Without the flag the same permission reaches the user as an ordinary open failure, so
    // the refusal above is attributable to `--delete-org` and not to the directory.
    assert!(!without.status.success());
    assert!(
        !String::from_utf8_lossy(&without.stderr).contains("cannot be identified"),
        "a run without the flag raised the flag's refusal"
    );
}

/// Both new flags' help states what they resolve to rather than what they are called.
///
/// The list test compares option names only, so it cannot catch a description that loses its
/// rule — and `error-presentation` will rewrite this text in a later Change. `--fix-idx`,
/// `--charset` and `--pwd` each set this precedent in their own suites.
#[test]
fn the_output_and_delete_flags_help_states_what_they_resolve_to() {
    let help = String::from_utf8(
        Command::new(BINARY)
            .arg("--help")
            .output()
            .expect("runs the binary")
            .stdout,
    )
    .expect("help is UTF-8");

    // Split on the flag name first, as the three precedents do: asserting against the whole
    // help output would pass if a clause migrated into another flag's description, into the
    // positional's, or into the command's `about` line — and `error-presentation` rewriting
    // this text in a later Change is exactly the edit that could move one without deleting it.
    let description = |flag: &str| {
        let at = help
            .find(flag)
            .unwrap_or_else(|| panic!("`{flag}` is not in the help: {help}"));
        let rest = &help[at + flag.len()..];
        // Every option's description ends where the next option's `      --` begins.
        rest.find("\n      -")
            .map_or(rest, |end| &rest[..end])
            .to_owned()
    };

    // `-o`: which value selects the location arm, and that a filename is not extended.
    let out = description("-o, --out");
    for clause in ["path separator", "existing directory", "verbatim"] {
        assert!(
            out.contains(clause),
            "`-o`'s own description does not say `{clause}`: {out}"
        );
    }
    // `--delete-org`: that the removal happens only once the output is in place.
    let delete = description("--delete-org ");
    for clause in ["once the output archive is in place", "if the run failed"] {
        assert!(
            delete.contains(clause),
            "`--delete-org`'s own description does not say `{clause}`: {delete}"
        );
    }
}
