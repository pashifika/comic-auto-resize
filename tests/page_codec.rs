//! The evidence that mozjpeg is linked and working on this target.
//!
//! A resolved dependency proves nothing: `mozjpeg-sys` builds a C library from source and
//! links it statically, so a failure is a link or an assembler failure and is specific to
//! the platform. These tests run natively on both release targets for that reason.

use std::fs;
use std::iter;
use std::path::PathBuf;

use comic_auto_resize::page::{
    Channels, DctMethod, DecodeSettings, EncodeSettings, Filter, PageErrorKind, PageImage,
    Resampler, decode, encode, height_for_width, scale_numerator,
};

/// Dimensions of `tests/fixtures/page.jpg`. See the note beside it.
const FIXTURE_WIDTH: u32 = 160;
const FIXTURE_HEIGHT: u32 = 240;

fn fixture() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/page.jpg");
    fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Three horizontal bands: hard vertical stripes, a horizontal gradient, then a flat
/// region. The same pattern the committed fixture carries, so a larger source can be
/// built for the cases the fixture is deliberately too small for.
///
/// Every pixel is a single grey level, so the same picture can be emitted as one channel
/// or as three identical ones and the two are comparable.
fn banded(width: u32, height: u32, channels: Channels) -> PageImage {
    let band = height / 3;
    let span = width.saturating_sub(1).max(1);
    let count = channels.count() as usize;
    let mut pixels = Vec::with_capacity(width as usize * height as usize * count);
    for y in 0..height {
        for x in 0..width {
            let value = if y < band {
                if (x / 2) % 2 == 0 { 0u8 } else { 255 }
            } else if y < band * 2 {
                // Integer arithmetic, rounded half up, so the pattern is identical on
                // every platform and can be reproduced without a float.
                let ramp = (x * 255 + span / 2) / span;
                u8::try_from(ramp).unwrap_or(255)
            } else {
                128
            };
            pixels.extend(iter::repeat_n(value, count));
        }
    }
    PageImage::new(width, height, channels, pixels)
        .expect("the buffer is built from the dimensions")
}

/// Resizes to `target_width`, panicking with the page's name on failure. The height is the
/// resampler's to derive; there is no second axis to pass.
fn resize_to_width(name: &str, source: &PageImage, target_width: u32, filter: Filter) -> PageImage {
    Resampler::new()
        .resize(name, source, target_width, filter)
        .unwrap_or_else(|error| panic!("{name} failed to resize: {error}"))
}

/// The offset of the start-of-frame segment, by walking the marker structure.
///
/// Not a byte scan for `FF C0`: a quantisation table forced into the baseline range holds
/// entries of exactly 255, so `FF` followed by an arbitrary quantiser byte appears inside
/// the `DQT` payload. Measured on the committed fixture at qualities 17 and 30, a scan that
/// only looked for `FF DA` to stop at read a quantiser pair as a start-of-scan and reported
/// no frame header at all. Each segment is therefore skipped by its declared length.
fn start_of_frame_offset(jpeg: &[u8]) -> Option<usize> {
    assert_eq!(&jpeg[..2], b"\xFF\xD8", "not a JPEG stream");

    let mut index = 2;
    while index + 3 < jpeg.len() {
        assert_eq!(
            jpeg[index], 0xFF,
            "expected a marker at offset {index}, found {:02X}",
            jpeg[index]
        );
        match jpeg[index + 1] {
            // Fill bytes are legal between segments.
            0xFF => index += 1,
            0xC0..=0xC2 => return Some(index),
            // Start of scan or end of image: there is no frame header.
            0xDA | 0xD9 => return None,
            _ => {
                let length = usize::from(u16::from_be_bytes([jpeg[index + 2], jpeg[index + 3]]));
                index += 2 + length;
            }
        }
    }
    None
}

/// The start-of-frame marker, which says whether a file is baseline (`C0`), extended
/// sequential (`C1`), or progressive (`C2`).
fn start_of_frame(jpeg: &[u8]) -> Option<u8> {
    start_of_frame_offset(jpeg).map(|offset| jpeg[offset + 1])
}

/// The component count declared in the start-of-frame header.
///
/// Read from the file rather than from our own types, so "one component" means one
/// component to any other decoder as well.
fn frame_components(jpeg: &[u8]) -> Option<u8> {
    // Marker (2), length (2), sample precision (1), height (2), width (2), then the count.
    start_of_frame_offset(jpeg).and_then(|offset| jpeg.get(offset + 9).copied())
}

#[test]
fn decode_resize_encode_round_trips() {
    let page = decode("page.jpg", &fixture(), DecodeSettings::default())
        .expect("the committed fixture decodes");
    assert_eq!(
        (page.width(), page.height()),
        (FIXTURE_WIDTH, FIXTURE_HEIGHT)
    );

    let resized = resize_to_width("page.jpg", &page, 1280, Filter::default());
    assert_eq!(resized.width(), 1280);

    let reencoded = encode("page.jpg", &resized, EncodeSettings::default())
        .expect("the resized page re-encodes");

    // Parsed back rather than merely non-empty: the width has to survive the whole trip.
    let parsed = decode("page.jpg", &reencoded, DecodeSettings::default())
        .expect("the re-encoded page decodes");
    assert_eq!(parsed.width(), 1280);
    assert_eq!(parsed.height(), resized.height());
}

#[test]
fn lower_quality_produces_a_smaller_file() {
    let page = decode("page.jpg", &fixture(), DecodeSettings::default()).expect("decodes");

    let high = encode("page.jpg", &page, EncodeSettings::default()).expect("encodes at 90");
    let low = encode(
        "page.jpg",
        &page,
        EncodeSettings {
            quality: 50,
            ..EncodeSettings::default()
        },
    )
    .expect("encodes at 50");

    assert!(
        low.len() < high.len(),
        "quality 50 produced {} bytes, quality 90 produced {}",
        low.len(),
        high.len()
    );
    assert!(decode("page.jpg", &low, DecodeSettings::default()).is_ok());
    assert!(decode("page.jpg", &high, DecodeSettings::default()).is_ok());
}

#[test]
fn entropy_coding_optimisation_is_honoured() {
    let page = decode("page.jpg", &fixture(), DecodeSettings::default()).expect("decodes");

    // Both baseline, so the scan script cannot confound the comparison: the only
    // difference between the two is whether the Huffman tables were optimised.
    let baseline = EncodeSettings {
        progressive: false,
        ..EncodeSettings::default()
    };
    let optimised = encode("page.jpg", &page, baseline).expect("encodes optimised");
    let plain = encode(
        "page.jpg",
        &page,
        EncodeSettings {
            optimize_coding: false,
            ..baseline
        },
    )
    .expect("encodes unoptimised");

    assert!(
        optimised.len() < plain.len(),
        "optimised produced {} bytes, unoptimised produced {}",
        optimised.len(),
        plain.len()
    );
}

#[test]
fn progressive_and_baseline_are_both_reachable() {
    let page = decode("page.jpg", &fixture(), DecodeSettings::default()).expect("decodes");

    // Every quality, not just the default. At quality 90 the quantisation table fits in 8
    // bits whether or not libjpeg was told to force it, so a baseline switch that is only
    // ever tested there passes while emitting `SOF1` — extended sequential, not baseline —
    // at every quality below 70. That is the coverage hole that let it through: measured
    // before the fix, `set_quality` gave `FFC1` for all of 1 to 69.
    for quality in 1..=100 {
        let baseline = encode(
            "page.jpg",
            &page,
            EncodeSettings {
                quality,
                progressive: false,
                ..EncodeSettings::default()
            },
        )
        .expect("encodes baseline");

        assert_eq!(
            start_of_frame(&baseline),
            Some(0xC0),
            "quality {quality} baseline: expected SOF0, not SOF1"
        );
        assert!(decode("page.jpg", &baseline, DecodeSettings::default()).is_ok());
    }

    // Progressive is structurally immune to the quantiser precision — libjpeg emits `SOF2`
    // whenever `progressive_mode` is set, before the baseline check runs — so the ends of
    // the scale are enough to show the switch still has two positions.
    for quality in [1, 50, 90, 100] {
        let progressive = encode(
            "page.jpg",
            &page,
            EncodeSettings {
                quality,
                ..EncodeSettings::default()
            },
        )
        .expect("encodes progressive");
        assert_eq!(
            start_of_frame(&progressive),
            Some(0xC2),
            "quality {quality} progressive: expected SOF2"
        );
    }
}

#[test]
fn a_quality_outside_the_documented_range_is_rejected_not_clamped() {
    let page = decode("page.jpg", &fixture(), DecodeSettings::default()).expect("decodes");

    let error = encode(
        "page.jpg",
        &page,
        EncodeSettings {
            quality: 0,
            ..EncodeSettings::default()
        },
    )
    .expect_err("libjpeg would have turned 0 into 1 without saying so");
    assert!(matches!(error.kind, PageErrorKind::Quality(0)));
    assert!(
        error.to_string().contains("page.jpg"),
        "the message must name the input: {error}"
    );
}

#[test]
fn the_dct_method_reaches_the_encoder() {
    let page = decode("page.jpg", &fixture(), DecodeSettings::default()).expect("decodes");

    let encoded = |method| {
        encode(
            "page.jpg",
            &page,
            EncodeSettings {
                dct_method: method,
                ..EncodeSettings::default()
            },
        )
        .unwrap_or_else(|error| panic!("{method:?} failed to encode: {error}"))
    };

    let slow = encoded(DctMethod::IntegerSlow);
    let fast = encoded(DctMethod::IntegerFast);
    let float = encoded(DctMethod::Float);
    assert!(!slow.is_empty() && !fast.is_empty() && !float.is_empty());

    // The load-bearing assertion. `Compress` exposes no DCT setter in the released crate,
    // so without the fork all three would be byte-identical: the library default would be
    // used every time. `ifast` is the approximate transform, so it is the one that must
    // differ; `islow` and `float` agree on 8-bit samples often enough that asserting all
    // three differ would be an assertion about this fixture rather than about the setting.
    assert_ne!(
        slow, fast,
        "islow and ifast produced identical bytes, so the setting did not reach libjpeg"
    );
}

#[test]
fn the_dct_method_reaches_the_decoder() {
    let jpeg = fixture();
    let decoded = |method| {
        decode(
            "page.jpg",
            &jpeg,
            DecodeSettings {
                dct_method: method,
                ..DecodeSettings::default()
            },
        )
        .unwrap_or_else(|error| panic!("{method:?} failed to decode: {error}"))
    };

    let slow = decoded(DctMethod::IntegerSlow);
    let fast = decoded(DctMethod::IntegerFast);
    let float = decoded(DctMethod::Float);

    for page in [&slow, &fast, &float] {
        assert_eq!(
            (page.width(), page.height()),
            (FIXTURE_WIDTH, FIXTURE_HEIGHT)
        );
    }
    assert_ne!(
        slow.pixels(),
        fast.pixels(),
        "islow and ifast produced identical pixels, so the setting did not reach libjpeg"
    );
}

#[test]
fn a_grayscale_page_stays_one_component_end_to_end() {
    let source = banded(320, 450, Channels::Gray);
    let jpeg = encode("gray.jpg", &source, EncodeSettings::default()).expect("encodes");
    assert_eq!(
        frame_components(&jpeg),
        Some(1),
        "a grayscale source must be written as a single-component frame"
    );

    let page = decode("gray.jpg", &jpeg, DecodeSettings::default()).expect("decodes");
    assert_eq!(page.channels(), Channels::Gray);
    assert_eq!((page.width(), page.height()), (320, 450));
    // One byte per pixel rather than three, which is the allocation half of the finding.
    assert_eq!(page.pixels().len(), 320 * 450);

    let resized = resize_to_width("gray.jpg", &page, 160, Filter::default());
    assert_eq!(resized.channels(), Channels::Gray);
    assert_eq!((resized.width(), resized.height()), (160, 225));
    assert_eq!(resized.pixels().len(), 160 * 225);

    let reencoded = encode("gray.jpg", &resized, EncodeSettings::default()).expect("re-encodes");
    assert_eq!(frame_components(&reencoded), Some(1));
    let parsed = decode("gray.jpg", &reencoded, DecodeSettings::default()).expect("decodes");
    assert_eq!(parsed.channels(), Channels::Gray);
    assert_eq!((parsed.width(), parsed.height()), (160, 225));
}

#[test]
fn a_colour_page_still_round_trips_as_three_components() {
    let source = banded(320, 450, Channels::Rgb);
    let jpeg = encode("colour.jpg", &source, EncodeSettings::default()).expect("encodes");
    assert_eq!(frame_components(&jpeg), Some(3));

    let page = decode("colour.jpg", &jpeg, DecodeSettings::default()).expect("decodes");
    assert_eq!(page.channels(), Channels::Rgb);
    assert_eq!(page.pixels().len(), 320 * 450 * 3);
}

#[test]
fn grayscale_costs_no_more_bytes_than_the_same_page_through_three_channels() {
    // The dimensions the finding was measured at, so the number in the report is the
    // number this asserts.
    let gray = encode(
        "gray.jpg",
        &banded(1280, 1800, Channels::Gray),
        EncodeSettings::default(),
    )
    .expect("encodes grayscale");
    let rgb = encode(
        "rgb.jpg",
        &banded(1280, 1800, Channels::Rgb),
        EncodeSettings::default(),
    )
    .expect("encodes the same picture as RGB");

    assert!(
        gray.len() <= rgb.len(),
        "grayscale produced {} bytes, three channels produced {}",
        gray.len(),
        rgb.len()
    );
}

#[test]
fn scaled_decode_picks_the_smallest_sufficient_step() {
    let source = banded(1520, 2150, Channels::Rgb);
    let jpeg = encode("big.jpg", &source, EncodeSettings::default()).expect("encodes");

    // 6/8 of 1520 is 1140, below the target; 7/8 is 1330, above it.
    assert_eq!(scale_numerator(1520, 2150, 1280, 1811), Some(7));

    let page = decode(
        "big.jpg",
        &jpeg,
        DecodeSettings {
            scale_to: Some((1280, 1811)),
            ..DecodeSettings::default()
        },
    )
    .expect("scaled decode");
    assert_eq!((page.width(), page.height()), (1330, 1882));
}

#[test]
fn scaled_decode_is_skipped_when_it_cannot_help() {
    let source = banded(1520, 2150, Channels::Rgb);
    let jpeg = encode("big.jpg", &source, EncodeSettings::default()).expect("encodes");

    let page = decode(
        "big.jpg",
        &jpeg,
        DecodeSettings {
            scale_to: Some((1520, 2150)),
            ..DecodeSettings::default()
        },
    )
    .expect("full-size decode");
    assert_eq!((page.width(), page.height()), (1520, 2150));
}

#[test]
fn the_final_height_follows_the_original_page_not_the_scaled_intermediate() {
    // 1463x1800 at width 1280 wants height 1575.
    let (source_width, source_height, target_width) = (1463, 1800, 1280);
    let target_height = height_for_width(source_width, source_height, target_width);
    assert_eq!(target_height, 1575);

    let jpeg = encode(
        "odd.jpg",
        &banded(source_width, source_height, Channels::Rgb),
        EncodeSettings::default(),
    )
    .expect("encodes");

    // libjpeg rounds each axis of a scaled decode up on its own, so the intermediate is
    // 1281 wide against 1575 high and no longer has the original's ratio.
    let intermediate = decode(
        "odd.jpg",
        &jpeg,
        DecodeSettings {
            scale_to: Some((target_width, target_height)),
            ..DecodeSettings::default()
        },
    )
    .expect("scaled decode");
    assert_eq!((intermediate.width(), intermediate.height()), (1281, 1575));
    // The intermediate remembers the page it was decoded from, which is the only reason
    // the original ratio is still recoverable at this point.
    assert_eq!(
        (
            intermediate.original_width(),
            intermediate.original_height()
        ),
        (source_width, source_height)
    );
    // Re-deriving from the intermediate's own dimensions is what produced the off-by-one.
    assert_eq!(
        height_for_width(intermediate.width(), intermediate.height(), target_width),
        1574
    );

    // No height is passed: the resampler takes it from the recorded original, so the
    // caller cannot reintroduce the off-by-one and cannot name a third answer either.
    let resized = Resampler::new()
        .resize("odd.jpg", &intermediate, target_width, Filter::default())
        .expect("resizes");
    assert_eq!((resized.width(), resized.height()), (1280, 1575));
}

#[test]
fn every_filter_resizes_and_lanczos2_is_not_lanczos3() {
    let page = decode("page.jpg", &fixture(), DecodeSettings::default()).expect("decodes");
    let mut resampler = Resampler::new();

    let mut outputs = Vec::new();
    for name in Filter::NAMES {
        let filter: Filter = name.parse().expect("an advertised name parses");
        let resized = resampler
            .resize("page.jpg", &page, 96, filter)
            .unwrap_or_else(|error| panic!("{name} failed to resize: {error}"));
        assert_eq!(resized.width(), 96, "{name} produced the wrong width");
        assert_eq!(resized.height(), 144, "{name} produced the wrong height");
        outputs.push((name, resized));
    }

    // The constructed Lanczos2 kernel has to reach the resizer. If the mapping silently
    // fell back to `Lanczos3`, these two would be identical.
    let lanczos2 = &outputs
        .iter()
        .find(|(name, _)| *name == "lanczos2")
        .expect("lanczos2 was resized")
        .1;
    let lanczos3 = &outputs
        .iter()
        .find(|(name, _)| *name == "lanczos3")
        .expect("lanczos3 was resized")
        .1;
    assert_ne!(
        lanczos2.pixels(),
        lanczos3.pixels(),
        "lanczos2 and lanczos3 produced identical pixels"
    );
}

#[test]
fn resizing_preserves_the_aspect_ratio() {
    let source = banded(1520, 2150, Channels::Rgb);
    let resized = resize_to_width("big.jpg", &source, 1280, Filter::default());
    assert_eq!((resized.width(), resized.height()), (1280, 1811));
}

#[test]
fn a_page_too_thin_to_round_to_a_row_keeps_one() {
    // 30000x1 at width 1280 rounds the height to 0. A zero-height destination is a
    // successful no-op inside `fast_image_resize`, so it used to come back `Ok` and fail
    // later as an "Empty JPEG image" blamed on the encoder.
    let source = banded(30000, 1, Channels::Rgb);
    let resized = resize_to_width("thin.jpg", &source, 1280, Filter::default());
    assert_eq!((resized.width(), resized.height()), (1280, 1));
    assert_eq!(resized.pixels().len(), 1280 * 3);

    // And it is a real page from there on: it encodes and decodes back at that size.
    let jpeg = encode("thin.jpg", &resized, EncodeSettings::default()).expect("encodes");
    let parsed = decode("thin.jpg", &jpeg, DecodeSettings::default()).expect("decodes");
    assert_eq!((parsed.width(), parsed.height()), (1280, 1));
}

#[test]
fn a_zero_target_axis_is_rejected_rather_than_returning_an_empty_page() {
    let mut resampler = Resampler::new();

    // A zero can now only arrive from the one axis a caller names, or from a source with
    // no extent on an axis, which leaves the derived height at 0. Both are rejected.
    let cases = [
        (banded(320, 450, Channels::Rgb), 0),
        (
            PageImage::new(0, 450, Channels::Rgb, Vec::new()).expect("0 * 450 * 3 is 0 bytes"),
            1280,
        ),
        (
            PageImage::new(320, 0, Channels::Rgb, Vec::new()).expect("320 * 0 * 3 is 0 bytes"),
            1280,
        ),
        (
            PageImage::new(0, 0, Channels::Rgb, Vec::new()).expect("0 * 0 * 3 is 0 bytes"),
            1280,
        ),
    ];

    for (source, target_width) in cases {
        let error = resampler
            .resize("page.jpg", &source, target_width, Filter::default())
            .expect_err("a zero-sized destination is not a resize");
        assert!(matches!(error.kind, PageErrorKind::Resize(_)));
        assert!(
            error.to_string().contains("page.jpg"),
            "the message must name the input: {error}"
        );
    }
}

/// The property the independent target height destroyed: a destination out of proportion
/// to its own source is not expressible.
///
/// `resize` takes no height. It derives one from the page geometry the source records, and
/// the only public way to build a `PageImage` records the buffer's own dimensions —
/// `PageImage::scaled_from`, which records anything else, is crate-private, so from out
/// here the line that would reproduce the finding does not compile at all. That is pinned
/// as a `compile_fail` doctest on `Resampler::resize`; what is left to check at runtime is
/// that the one axis a caller does name still yields a destination fixed by the source.
#[test]
fn a_tiny_source_cannot_reach_a_destination_out_of_proportion_to_itself() {
    // Three bytes. With an independent target height of `u32::MAX`, this same source
    // reached `fast_image_resize`'s infallible `vec![0; 1280 * 4_294_967_295 * 3]`.
    let tiny = PageImage::new(1, 1, Channels::Rgb, vec![0; 3]).expect("1 * 1 * 3");
    assert_eq!(
        (tiny.original_width(), tiny.original_height()),
        (1, 1),
        "a publicly built page is its own original"
    );

    let resized = resize_to_width("tiny.jpg", &tiny, 1280, Filter::default());
    assert_eq!((resized.width(), resized.height()), (1280, 1280));
    // The 4,915,200 bytes the pre-round-2 code was structurally limited to, and no more.
    assert_eq!(resized.pixels().len(), 1280 * 1280 * 3);

    // The result is its own original too, so resizing again is bounded by the same rule
    // rather than compounding whatever it was derived from.
    assert_eq!(
        (resized.original_width(), resized.original_height()),
        (1280, 1280)
    );
}

#[test]
fn input_that_is_not_a_jpeg_is_rejected_before_libjpeg() {
    let png_magic = b"\x89PNG\r\n\x1a\n";
    let error = decode("page.png", png_magic, DecodeSettings::default())
        .expect_err("a PNG header is not a JPEG");

    assert!(matches!(error.kind, PageErrorKind::NotJpeg(_)));
    assert_eq!(error.name, "page.png");
    assert!(
        error.to_string().contains("page.png"),
        "the message must name the input: {error}"
    );
}

#[test]
fn a_jpeg_truncated_inside_its_headers_is_reported() {
    let jpeg = fixture();
    let error = decode("page.jpg", &jpeg[..40], DecodeSettings::default())
        .expect_err("a truncated header cannot be decoded");

    assert!(matches!(error.kind, PageErrorKind::Decode(_)));
    assert!(
        error.to_string().contains("page.jpg"),
        "the message must name the input: {error}"
    );
}

/// Pins a limitation rather than a feature.
///
/// mozjpeg's source manager synthesises an end-of-image marker when the input runs out, so
/// a JPEG truncated after its headers decodes to a partially filled image instead of
/// failing. Truncation is one case of a general behaviour: libjpeg recovers from damaged
/// entropy data by substituting fabricated coefficients and continuing, and mozjpeg
/// silences the warnings that say so, so any such page returns `Ok`. The Go implementation
/// had the same behaviour, through the same library. This test exists so that the day it
/// changes is a deliberate day.
#[test]
fn a_jpeg_truncated_after_its_headers_decodes_partially() {
    let jpeg = fixture();
    let page = decode(
        "page.jpg",
        &jpeg[..jpeg.len() * 3 / 4],
        DecodeSettings::default(),
    )
    .expect("libjpeg fakes an end-of-image marker rather than failing");

    assert_eq!(
        (page.width(), page.height()),
        (FIXTURE_WIDTH, FIXTURE_HEIGHT)
    );
}

#[test]
fn the_process_survives_a_decode_failure() {
    let jpeg = fixture();

    // Ten failures in a row, then a success: the unwind out of C is caught each time and
    // leaves nothing behind.
    for _ in 0..10 {
        assert!(decode("bad.jpg", &jpeg[..40], DecodeSettings::default()).is_err());
    }
    assert!(decode("page.jpg", &jpeg, DecodeSettings::default()).is_ok());
}

#[test]
fn the_process_survives_an_encode_failure() {
    // libjpeg's `JERR_EMPTY_IMAGE`: `jpeg_start_compress` rejects a zero dimension by
    // unwinding out of C. This is the reachable case for the encoder's `catch_unwind`,
    // which otherwise has no test at all.
    let empty = PageImage::new(0, 0, Channels::Rgb, Vec::new()).expect("0 * 0 * 3 is 0 bytes");
    let good = banded(64, 96, Channels::Rgb);

    for _ in 0..10 {
        let error = encode("empty.jpg", &empty, EncodeSettings::default())
            .expect_err("libjpeg refuses an image with no pixels");
        assert!(
            matches!(error.kind, PageErrorKind::Encode(_)),
            "expected an encode failure, got {:?}",
            error.kind
        );
        assert!(
            error.to_string().contains("empty.jpg"),
            "the message must name the input: {error}"
        );
    }

    // The unwind left nothing behind: a valid page still encodes afterwards.
    assert!(encode("good.jpg", &good, EncodeSettings::default()).is_ok());
}
