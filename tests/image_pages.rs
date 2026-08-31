//! A page that is not a JPEG, from the probe through to the output archive.
//!
//! The defect this closes lost content silently: the extension filter passed over any entry
//! no candidate claimed, so a png page never reached a decoder and the run reported success
//! one page short. `samples/…_日本語フォルダ3.zip` is the archive it happens to — one `.jpg`,
//! six `.png` — and the acceptance case is that it goes from one page to seven.
//!
//! Fixtures are generated rather than committed: `image`'s png, bmp and webp *encoders* are
//! under the same features as its decoders, so the only corpus that has to be fetched is the
//! BMP conformance suite, and the tests that need it skip with a message naming the script.

mod support;

use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::Command;

use comic_auto_resize::page::{
    Budget, Channels, DecodeSettings, EncodeSettings, Filter, Format, PageErrorKind, decode, header,
};
use comic_auto_resize::pipeline::{self, RunError, Settings};
use comic_auto_resize::policy::AUTO_WIDTH;
use comic_auto_resize::source::{ReadOptions, SourceError, ZipSource, probe};

use support::{
    TempDir, animated_webp, apng_page, bmp_fixtures, bmp_page, jpeg_size, page, page_bytes,
    png_gray_page, png_half_transparent_page, png_page, png_rgb16_page, png_rgba_page,
    png_rgba16_page, read_archive, webp_page, write_archive,
};

/// The binary under test, built by Cargo for this integration test at the same profile.
const BINARY: &str = env!("CARGO_BIN_EXE_comic-auto-resize");

/// Fixture geometry. Wide enough that the normalising resize does something, small enough
/// that a test that decodes forty of them stays fast.
const WIDTH: u32 = 200;
const HEIGHT: u32 = 300;

fn settings() -> Settings {
    Settings {
        jobs: NonZeroUsize::new(2).expect("non-zero"),
        target_width: AUTO_WIDTH,
        filter: Filter::default(),
        decode: DecodeSettings::default(),
        encode: EncodeSettings::default(),
    }
}

/// Runs the pipeline over an in-memory archive.
fn run(input: &[u8], output: &Path) -> Result<pipeline::Report, RunError> {
    let source = ZipSource::new(
        std::io::Cursor::new(input.to_vec()),
        &ReadOptions::default(),
    )?;
    pipeline::run(source, output, &settings())
}

/// A zip holding `entries`, as `(name, bytes)`.
fn archive(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let directory = TempDir::new("image-pages-archive");
    let path = directory.path().join("in.zip");
    let owned: Vec<_> = entries
        .iter()
        .map(|(name, bytes)| ((*name).to_owned(), bytes.clone()))
        .collect();
    write_archive(&path, &owned);
    fs::read(&path).expect("the fixture archive reads back")
}

/// The pixels of `buffer` decoded as `format`, with whether the decoder composited.
fn pixels(name: &str, buffer: &[u8], format: Format) -> (Channels, Vec<u8>, bool) {
    let decoded = decode(name, buffer, format, DecodeSettings::default())
        .unwrap_or_else(|error| panic!("{name} decodes: {error}"));
    (
        decoded.page.channels(),
        decoded.page.pixels().to_vec(),
        decoded.composited,
    )
}

// ---------------------------------------------------------------------------
// Every format is a page
// ---------------------------------------------------------------------------

/// The whole of the Change in one assertion: four formats in, four JPEG pages out, under the
/// naming rule that already existed.
#[test]
fn an_entry_in_any_supported_format_is_a_page() {
    let directory = TempDir::new("image-pages-all-formats");
    let output = directory.path().join("out.zip");
    let input = archive(&[
        ("pages/page1.jpg", page_bytes(WIDTH, HEIGHT)),
        ("pages/page2.png", png_page(WIDTH, HEIGHT)),
        ("pages/page3.bmp", bmp_page(WIDTH, HEIGHT)),
        ("pages/page4.webp", webp_page(WIDTH, HEIGHT)),
    ]);

    let report = run(&input, &output).expect("every format is a page");
    assert_eq!(report.pages, 4);
    assert_eq!(
        report.composited, 0,
        "none of these carries an alpha channel"
    );

    let written = read_archive(&output);
    let names: Vec<&str> = written.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "pages/page1.jpg",
            "pages/page2.jpg",
            "pages/page3.jpg",
            "pages/page4.jpg"
        ],
        "one encoder, so every entry leaves as .jpg"
    );
    // Every output really is a JPEG at the normalised width, not merely an entry with the
    // right name.
    for (name, bytes) in &written {
        assert_eq!(
            jpeg_size(bytes).map(|(width, _)| width),
            Some(WIDTH),
            "{name} is not a JPEG of the expected width"
        );
    }
}

/// The corpus defect, reproduced at the shape the sample has: one jpg and six png in one
/// archive used to write a one-page book and exit 0.
#[test]
fn a_png_page_is_no_longer_passed_over_in_silence() {
    let directory = TempDir::new("image-pages-defect");
    let output = directory.path().join("out.zip");
    let mut entries = vec![("cover.jpg", page_bytes(WIDTH, HEIGHT))];
    let png = png_page(WIDTH, HEIGHT);
    let names: Vec<String> = (1..=6).map(|index| format!("page{index}.png")).collect();
    entries.extend(names.iter().map(|name| (name.as_str(), png.clone())));

    let report = run(&archive(&entries), &output).expect("seven pages");
    assert_eq!(report.pages, 7, "one page short is the failure this closes");
    assert_eq!(read_archive(&output).len(), 7);
}

/// The extension filter and the candidate list stay the same length, and an entry outside
/// both is still passed over rather than reported as a disagreement.
#[test]
fn an_entry_no_candidate_claims_is_still_passed_over() {
    let directory = TempDir::new("image-pages-filter");
    let output = directory.path().join("out.zip");
    let input = archive(&[
        ("ComicInfo.xml", b"<?xml version=\"1.0\"?>".to_vec()),
        ("page1.png", png_page(WIDTH, HEIGHT)),
        ("Thumbs.db", vec![0; 64]),
    ]);

    let report = run(&input, &output).expect("the non-image entries are skipped");
    assert_eq!(report.pages, 1);
}

/// A `.png` holding JPEG bytes is the archive contradicting itself, and it stays an error
/// rather than becoming a silent re-classification now that both formats are supported.
#[test]
fn an_extension_claiming_one_format_over_another_formats_bytes_is_an_error() {
    let directory = TempDir::new("image-pages-mismatch");
    let output = directory.path().join("out.zip");
    let input = archive(&[("page1.png", page_bytes(WIDTH, HEIGHT))]);

    let error = run(&input, &output).expect_err("the bytes are a JPEG, the name says PNG");
    let RunError::Source(SourceError::Mismatch { name, declared }) = error else {
        panic!("expected a mismatch, got {error:?}");
    };
    assert_eq!(name, "page1.png");
    assert_eq!(declared, "PNG");
    assert!(!output.exists(), "a failed run leaves no archive");
}

// ---------------------------------------------------------------------------
// Channels the encoder cannot carry
// ---------------------------------------------------------------------------

/// A single-component png stays single-component, as a grayscale JPEG does: widening grey to
/// RGB triples every buffer and enlarges the output.
#[test]
fn a_grayscale_png_stays_one_component() {
    let (channels, pixels, composited) =
        pixels("grey.png", &png_gray_page(WIDTH, HEIGHT), Format::Png);
    assert_eq!(channels, Channels::Gray);
    assert_eq!(pixels.len(), (WIDTH * HEIGHT) as usize);
    assert!(!composited);
}

/// The case the composite exists to get right. An alpha channel that is opaque everywhere
/// must leave every colour value where it was — the failure here would be silent, shifting
/// every pixel of every such page by a little.
#[test]
fn a_fully_opaque_alpha_channel_leaves_the_colour_values_unchanged() {
    let (_, opaque, composited) = pixels(
        "opaque.png",
        &png_rgba_page(WIDTH, HEIGHT, u8::MAX),
        Format::Png,
    );
    let (_, reference, _) = pixels("plain.png", &png_page(WIDTH, HEIGHT), Format::Png);

    assert_eq!(opaque, reference, "an opaque composite changed the pixels");
    assert_eq!(opaque, page(WIDTH, HEIGHT).pixels(), "and the generator's");
    assert!(
        composited,
        "the rule applied, so the run counts it — deciding otherwise would cost a full pixel \
         pass to find out whether any pixel was transparent"
    );
}

/// A transparent region becomes paper and an opaque one does not, in the same page. One
/// fixture rather than two, because the property is that the halves come out *differently*.
#[test]
fn a_transparent_region_becomes_white_and_an_opaque_one_is_untouched() {
    let (channels, pixels, composited) = pixels(
        "half.png",
        &png_half_transparent_page(WIDTH, HEIGHT),
        Format::Png,
    );
    assert_eq!(channels, Channels::Rgb);
    assert!(composited);

    let reference = page(WIDTH, HEIGHT);
    let reference = reference.pixels();
    let stride = WIDTH as usize * 3;
    let half = WIDTH as usize / 2 * 3;
    for y in 0..HEIGHT as usize {
        let row = &pixels[y * stride..(y + 1) * stride];
        assert!(
            row[..half].iter().all(|&sample| sample == u8::MAX),
            "row {y}'s transparent half is not white"
        );
        assert_eq!(
            &row[half..],
            &reference[y * stride + half..(y + 1) * stride],
            "row {y}'s opaque half was altered"
        );
    }
}

/// Sixteen bits narrow to eight, and nothing is reported: every JPEG encoder does this to
/// every deeper source, so reporting it would report a property of the output format rather
/// than a decision taken about the page.
#[test]
fn a_sixteen_bit_page_is_narrowed_without_being_counted() {
    let (channels, narrowed, composited) =
        pixels("deep.png", &png_rgb16_page(WIDTH, HEIGHT), Format::Png);
    assert_eq!(channels, Channels::Rgb);
    assert!(
        !composited,
        "narrowing is not the counted outcome; compositing is"
    );
    // The fixture widened each sample by `* 257`, so the high byte is the generator's own
    // value and the narrowing is exact against it.
    assert_eq!(narrowed, page(WIDTH, HEIGHT).pixels());
}

/// Both rules at once, which is what a 16-bit png with alpha needs: composited at the
/// source's depth, then narrowed once.
#[test]
fn a_sixteen_bit_page_with_an_opaque_alpha_channel_takes_both_rules() {
    let (channels, pixels, composited) = pixels(
        "deep-opaque.png",
        &png_rgba16_page(WIDTH, HEIGHT, u16::MAX),
        Format::Png,
    );
    assert_eq!(channels, Channels::Rgb);
    assert!(composited);
    assert_eq!(pixels, page(WIDTH, HEIGHT).pixels());
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// Out of scope is not the same as taking one frame of an animation as the page.
#[test]
fn a_multi_frame_input_is_refused_by_name() {
    let apng = apng_page(WIDTH, HEIGHT);
    let error = decode("anim.png", &apng, Format::Png, DecodeSettings::default())
        .expect_err("an APNG is an animation, not a page");
    assert!(
        matches!(
            error.kind,
            PageErrorKind::MultiFrame {
                format: Format::Png
            }
        ),
        "{:?}",
        error.kind
    );
    assert!(
        error.to_string().contains("anim.png") && error.to_string().contains("PNG"),
        "the refusal names the entry and the format: {error}"
    );

    // And the header read refuses it too, so the run stops before any decode is attempted.
    assert!(header("anim.png", &apng, Format::Png).is_err());

    let webp = animated_webp(WIDTH, HEIGHT);
    let error = decode("anim.webp", &webp, Format::WebP, DecodeSettings::default())
        .expect_err("an animated webp is an animation, not a page");
    assert!(
        matches!(
            error.kind,
            PageErrorKind::MultiFrame {
                format: Format::WebP
            }
        ),
        "{:?}",
        error.kind
    );
}

/// The refusal has to say what the decoder could not read, rather than that the entry was
/// not an image.
#[test]
fn a_refusal_names_the_format_and_carries_the_decoders_reason() {
    let mut truncated = png_page(WIDTH, HEIGHT);
    truncated.truncate(20);
    let error = decode(
        "short.png",
        &truncated,
        Format::Png,
        DecodeSettings::default(),
    )
    .expect_err("twenty bytes is not a png");

    let PageErrorKind::Decode { format, reason } = &error.kind else {
        panic!("expected a decode refusal, got {:?}", error.kind);
    };
    assert_eq!(*format, Format::Png);
    assert!(!reason.is_empty(), "the decoder's own message is carried");
    assert!(
        error.to_string().contains("short.png") && error.to_string().contains("PNG"),
        "{error}"
    );
}

/// The oversize refusal comes from the *declared* dimensions, for every format — including
/// the three whose decoder cannot produce a reduced image, where nothing else reads the
/// header first and the check has to be asked for rather than falling out of choosing a
/// scale. `source pixels` is the quantity only a header read establishes, so asserting on it
/// is what proves the refusal preceded the decode.
#[test]
fn the_refusal_is_on_the_declared_pixels_rather_than_on_the_decoded_buffer() {
    let settings = DecodeSettings {
        budget: Budget::new(1_000, 1 << 30),
        ..DecodeSettings::default()
    };

    for (name, bytes, format) in [
        ("page.jpg", page_bytes(WIDTH, HEIGHT), Format::Jpeg),
        ("page.png", png_page(WIDTH, HEIGHT), Format::Png),
        ("page.bmp", bmp_page(WIDTH, HEIGHT), Format::Bmp),
        ("page.webp", webp_page(WIDTH, HEIGHT), Format::WebP),
    ] {
        let error = decode(name, &bytes, format, settings)
            .expect_err("60,000 pixels is over a 1,000 pixel limit");
        let PageErrorKind::TooLarge {
            quantity, actual, ..
        } = error.kind
        else {
            panic!("expected a budget refusal for {name}, got {:?}", error.kind);
        };
        assert_eq!(
            quantity, "source pixels",
            "{name} was refused from the decoded buffer rather than from its header"
        );
        assert_eq!(actual, u128::from(WIDTH * HEIGHT));
    }
}

/// The decoded buffer is bounded too, and for the formats that cannot scale it is the term
/// that matters: an `Rgba16` page asks for eight bytes a pixel where the page it becomes
/// needs three.
#[test]
fn the_decoders_own_buffer_is_bounded_before_it_is_allocated() {
    let settings = DecodeSettings {
        budget: Budget::new(1 << 30, 1_000),
        ..DecodeSettings::default()
    };
    let error = decode(
        "deep.png",
        &png_rgba16_page(WIDTH, HEIGHT, u16::MAX),
        Format::Png,
        settings,
    )
    .expect_err("480,000 bytes is over a 1,000 byte limit");

    let PageErrorKind::TooLarge {
        quantity, actual, ..
    } = error.kind
    else {
        panic!("expected a budget refusal, got {:?}", error.kind);
    };
    assert_eq!(quantity, "decoded bytes");
    // Eight bytes a pixel, which is the decoder's figure and not the page's three.
    assert_eq!(actual, u128::from(WIDTH * HEIGHT * 8));
}

// ---------------------------------------------------------------------------
// The counted outcome
// ---------------------------------------------------------------------------

/// One count for the run, whatever the page count. A per-page line would emit two hundred
/// notices on a real archive and bury the number the user came for.
#[test]
fn the_run_counts_composited_pages_once() {
    let directory = TempDir::new("image-pages-count");
    let output = directory.path().join("out.zip");
    let transparent = png_half_transparent_page(WIDTH, HEIGHT);
    let entries: Vec<(String, Vec<u8>)> = (1..=5)
        .map(|index| (format!("page{index}.png"), transparent.clone()))
        .chain(std::iter::once((
            "page6.jpg".to_owned(),
            page_bytes(WIDTH, HEIGHT),
        )))
        .collect();
    let borrowed: Vec<(&str, Vec<u8>)> = entries
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.clone()))
        .collect();

    let report = run(&archive(&borrowed), &output).expect("six pages");
    assert_eq!(report.pages, 6);
    assert_eq!(report.composited, 5, "the JPEG has no alpha channel");
}

/// Counting is not a way to continue past a failure: a page the tool cannot process still
/// ends the run, and no archive is left behind.
#[test]
fn a_counted_outcome_is_not_a_swallowed_failure() {
    let directory = TempDir::new("image-pages-failure");
    let output = directory.path().join("out.zip");
    let mut damaged = png_page(WIDTH, HEIGHT);
    damaged.truncate(damaged.len() / 2);
    let input = archive(&[
        ("page1.png", png_half_transparent_page(WIDTH, HEIGHT)),
        ("page2.png", damaged),
    ]);

    let error = run(&input, &output).expect_err("a page that cannot be decoded ends the run");
    let RunError::Page(page) = error else {
        panic!("expected a page failure, got {error:?}");
    };
    // The output name, which is the name the reader handed on: the extension is rewritten
    // before the page reaches a worker, so a refusal names the entry as the output would
    // have held it.
    assert_eq!(page.name, "page2.jpg");
    assert!(!output.exists(), "a failed run leaves no archive");
}

/// A run that composited nothing produces the output it produced before the rule existed,
/// and a run that composited something says so once. Through the binary, because the line is
/// the binary's.
#[test]
fn the_summary_line_mentions_compositing_only_when_it_happened() {
    let directory = TempDir::new("image-pages-summary");

    let plain = directory.path().join("plain.zip");
    write_archive(&plain, &[("page1.png".to_owned(), png_page(WIDTH, HEIGHT))]);
    let output = String::from_utf8(
        Command::new(BINARY)
            .arg(&plain)
            .output()
            .expect("the binary runs")
            .stdout,
    )
    .expect("stdout is UTF-8");
    assert_eq!(
        output.lines().count(),
        1,
        "the success line is one line: {output}"
    );
    assert!(output.contains("1 page(s) written to"), "{output}");
    assert!(
        !output.contains("composited"),
        "a run that composited nothing says nothing about compositing: {output}"
    );

    let transparent = directory.path().join("alpha.zip");
    write_archive(
        &transparent,
        &[
            (
                "page1.png".to_owned(),
                png_half_transparent_page(WIDTH, HEIGHT),
            ),
            ("page2.jpg".to_owned(), page_bytes(WIDTH, HEIGHT)),
        ],
    );
    let output = String::from_utf8(
        Command::new(BINARY)
            .arg(&transparent)
            .output()
            .expect("the binary runs")
            .stdout,
    )
    .expect("stdout is UTF-8");
    assert_eq!(output.lines().count(), 1, "still one line: {output}");
    assert!(output.contains("2 page(s) written to"), "{output}");
    assert!(
        output.contains("1 page(s) composited onto white"),
        "the count reaches the user: {output}"
    );
}

// ---------------------------------------------------------------------------
// The BMP conformance corpus
// ---------------------------------------------------------------------------

/// The 89-file outcome table, committed so a dependency bump that moves one file's behaviour
/// is visible rather than absorbed.
///
/// Skipped with a message naming the script when the corpus is absent, which is the
/// convention the rar and 7z suites follow. CI never fetches it: png and webp cover the
/// decoders everywhere, and this covers the format whose matrix is the reason `image` was
/// chosen over a hand-written reader.
#[test]
fn the_bmp_conformance_outcomes_are_the_recorded_ones() {
    let Ok(root) = bmp_fixtures() else {
        eprintln!("skipped: {}", bmp_fixtures().unwrap_err());
        return;
    };

    let expected = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bmp-outcomes.tsv"),
    )
    .expect("the recorded table is committed");

    let mut moved = Vec::new();
    let mut counts = [(0, 0); 4];
    for line in expected
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
    {
        let (file, outcome) = line
            .split_once('\t')
            .unwrap_or_else(|| panic!("a table row is `path<TAB>reads|refused`: {line}"));
        let bytes = fs::read(root.join(file)).unwrap_or_else(|error| panic!("{file}: {error}"));
        let read = decode(file, &bytes, Format::Bmp, DecodeSettings::default()).is_ok();
        let now = if read { "reads" } else { "refused" };
        if now != outcome {
            moved.push(format!("{file}: recorded {outcome}, now {now}"));
        }

        let set = match file.as_bytes()[0] {
            b'g' => 0,
            b'q' => 1,
            b'b' => 2,
            _ => 3,
        };
        if read {
            counts[set].0 += 1;
        } else {
            counts[set].1 += 1;
        }
    }

    assert!(moved.is_empty(), "{}", moved.join("\n"));
    // The table's own totals, so a row silently dropped from the file is caught too.
    assert_eq!(
        counts,
        [(27, 0), (31, 10), (5, 15), (0, 1)],
        "g/, q/, b/, x/ as (reads, refused)"
    );
}

/// A sub-format this build cannot read is refused **by name**, and the test asserts the name
/// reaches the user rather than that a refusal happened. These formats are families — BMP
/// alone has seven compression schemes and five header generations — so a build that carries
/// the common ones and refuses the rest is the ordinary case, provided the refusal says
/// which.
#[test]
fn a_sub_format_this_build_cannot_read_is_refused_naming_its_feature() {
    let Ok(root) = bmp_fixtures() else {
        eprintln!("skipped: {}", bmp_fixtures().unwrap_err());
        return;
    };

    // Each pair is a file and a phrase the refusal must contain. The phrases are the
    // decoder's own, which is why they are worth pinning: they are what a user is told.
    for (file, feature) in [
        ("q/rgb24jpeg.bmp", "JPEG compression"),
        ("q/rgb24png.bmp", "PNG compression"),
        ("q/rgba64.bmp", "bit count"),
        ("q/rgba32abf.bmp", "compression type"),
        ("q/pal8oversizepal.bmp", "Palette size"),
        ("q/pal1huffmsb.bmp", "Unknown bitmap"),
        ("q/pal8os2v2.bmp", "Unknown bitmap"),
    ] {
        let bytes = fs::read(root.join(file)).unwrap_or_else(|error| panic!("{file}: {error}"));
        let error = decode(file, &bytes, Format::Bmp, DecodeSettings::default())
            .expect_err("this build cannot read this sub-format");
        let message = error.to_string();
        assert!(
            message.contains(file) && message.contains("BMP") && message.contains(feature),
            "the refusal must name the entry, the format and what it could not read: {message}"
        );
    }
}

/// Why the five invalid bmps that read are accepted, asserted rather than inherited: their
/// invalidity is in metadata the decoder does not consult, so every one decodes at the same
/// 127×64 geometry as the good files. If that stops being true the acceptance has to be
/// revisited.
#[test]
fn the_invalid_bmps_that_read_do_so_at_the_right_geometry() {
    let Ok(root) = bmp_fixtures() else {
        eprintln!("skipped: {}", bmp_fixtures().unwrap_err());
        return;
    };

    for file in [
        "b/badfilesize.bmp",
        "b/badbitssize.bmp",
        "b/baddens1.bmp",
        "b/baddens2.bmp",
        "b/pal8badindex.bmp",
    ] {
        let bytes = fs::read(root.join(file)).unwrap_or_else(|error| panic!("{file}: {error}"));
        assert_eq!(
            header(file, &bytes, Format::Bmp).expect("the header reads"),
            (127, 64),
            "{file}'s declared geometry moved"
        );
        let decoded = decode(file, &bytes, Format::Bmp, DecodeSettings::default())
            .unwrap_or_else(|error| panic!("{file}: {error}"));
        assert_eq!(
            (decoded.page.width(), decoded.page.height()),
            (127, 64),
            "{file} decoded at the wrong size, which would be silent output damage"
        );
        assert_eq!(decoded.page.channels(), Channels::Rgb);
        assert!(!decoded.composited);
    }
}

/// `b/pal8badindex.bmp` holds palette indices past the end of its own palette, and the survey
/// left the pixel-level answer unmeasured. Measured here: `image` **zero-fills** its palette
/// to 256 entries (`read_palette`, "Allocate 256 entries even if `palette_size` is smaller"),
/// so an out-of-range index resolves to black — not clamped to the last entry, not wrapped.
///
/// **This overturns the design's stated basis for accepting the file.** The five `b/` files
/// were accepted because "their invalidity is in metadata the decoder does not consult"; for
/// this one the invalidity is in the *pixel data* and the decoder does consult it, so 4,793
/// of its 8,128 pixels — 59% of the page — come out black. That is visibly wrong output, not
/// merely an invalid file that reads.
///
/// It is accepted anyway, and the reason is cost rather than the original argument: detecting
/// it needs the declared palette size and a pass over the index plane, which is a second BMP
/// reader in miniature — the thing choosing `image` over a hand-written decoder exists to
/// avoid — and `image`'s API cannot report the declared size, because the palette it hands
/// back is always 256 entries. No real comic archive holds such a file; this one exists
/// because a conformance suite's author built it.
///
/// So the behaviour is pinned exactly rather than described, by decoding the file
/// independently here: if `image` ever starts clamping or wrapping, this fails and the
/// accept/refuse decision is retaken with a measurement in hand.
#[test]
fn an_out_of_range_palette_index_decodes_as_black() {
    let Ok(root) = bmp_fixtures() else {
        eprintln!("skipped: {}", bmp_fixtures().unwrap_err());
        return;
    };

    let bytes = fs::read(root.join("b/pal8badindex.bmp")).expect("the fixture reads");
    let (channels, decoded, _) = pixels("b/pal8badindex.bmp", &bytes, Format::Bmp);
    assert_eq!(channels, Channels::Rgb);

    // The file's own header, read here rather than through `image`: 8 bits per pixel, a
    // `BITMAPINFOHEADER`, and `biClrUsed` palette entries of four bytes each.
    let le32 = |at: usize| {
        usize::try_from(u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()))
            .expect("a fixture header field fits")
    };
    let offset = le32(10);
    let header_size = le32(14);
    let width = le32(18);
    let height = le32(22);
    let bits = u16::from_le_bytes(bytes[28..30].try_into().unwrap());
    let entries = le32(46);
    assert_eq!((bits, header_size, width, height), (8, 40, 127, 64));
    assert_eq!(entries, 101, "the palette this file declares");

    // Every index, from the bottom-up rows the format stores, resolved against a palette
    // zero-filled to 256 entries — which is exactly what `image` does.
    let palette_at = 14 + header_size;
    let stride = (width * usize::from(bits)).div_ceil(32) * 4;
    let mut expected = Vec::with_capacity(width * height * 3);
    let mut out_of_range = 0;
    for y in 0..height {
        let row = offset + (height - 1 - y) * stride;
        for index in &bytes[row..row + width] {
            let index = usize::from(*index);
            if index >= entries {
                out_of_range += 1;
                expected.extend_from_slice(&[0, 0, 0]);
            } else {
                let entry = palette_at + index * 4;
                // Stored BGRA; the alpha byte is not a palette channel.
                expected.extend_from_slice(&[bytes[entry + 2], bytes[entry + 1], bytes[entry]]);
            }
        }
    }

    assert_eq!(out_of_range, 4_793, "the file's out-of-range index count");
    assert_eq!(
        decoded, expected,
        "an out-of-range index no longer decodes as black; retake the accept decision"
    );
}

/// Why BMP's magic is ten bytes rather than two, against the real file. `x/ba-bm.bmp` is an
/// OS/2 bitmap *array*: it begins `BA` and contains a `BM`, so a two-byte magic would hand it
/// to the BMP decoder on the strength of nothing.
#[test]
fn an_os2_bitmap_array_is_not_claimed_by_the_bmp_candidate() {
    let Ok(root) = bmp_fixtures() else {
        eprintln!("skipped: {}", bmp_fixtures().unwrap_err());
        return;
    };

    let bytes = fs::read(root.join("x/ba-bm.bmp")).expect("the fixture reads");
    assert_eq!(&bytes[..2], b"BA", "the fixture is a bitmap array");
    assert!(
        bytes.windows(2).any(|window| window == b"BM"),
        "and it does contain a BM"
    );
    assert_eq!(
        probe(&bytes),
        None,
        "no candidate may claim an OS/2 bitmap array"
    );
}
