//! The evidence that mozjpeg is linked and working on this target.
//!
//! A resolved dependency proves nothing: `mozjpeg-sys` builds a C library from source and
//! links it statically, so a failure is a link or an assembler failure and is specific to
//! the platform. These tests run natively on both release targets for that reason.

use std::fs;
use std::path::PathBuf;

use comic_auto_resize::page::{
    DctMethod, DecodeSettings, EncodeSettings, Filter, PageErrorKind, Resampler, RgbImage, decode,
    encode, scale_numerator,
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
fn banded(width: u32, height: u32) -> RgbImage {
    let band = height / 3;
    let span = width.saturating_sub(1).max(1);
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 3);
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
            pixels.extend_from_slice(&[value, value, value]);
        }
    }
    RgbImage::new(width, height, pixels).expect("the buffer is built from the dimensions")
}

/// The start-of-frame marker, which says whether a file is baseline or progressive.
///
/// Scanned up to the first start-of-scan, so entropy-coded data can never be mistaken for
/// a marker.
fn start_of_frame(jpeg: &[u8]) -> Option<u8> {
    let mut index = 0;
    while index + 1 < jpeg.len() {
        if jpeg[index] == 0xFF {
            match jpeg[index + 1] {
                marker @ 0xC0..=0xC2 => return Some(marker),
                0xDA => return None,
                _ => {}
            }
        }
        index += 1;
    }
    None
}

#[test]
fn decode_resize_encode_round_trips() {
    let page = decode("page.jpg", &fixture(), DecodeSettings::default())
        .expect("the committed fixture decodes");
    assert_eq!(
        (page.width(), page.height()),
        (FIXTURE_WIDTH, FIXTURE_HEIGHT)
    );

    let resized = Resampler::new()
        .resize("page.jpg", &page, 1280, Filter::default())
        .expect("the fixture resizes");
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
fn progressive_and_baseline_are_both_reachable() {
    let page = decode("page.jpg", &fixture(), DecodeSettings::default()).expect("decodes");

    let progressive = encode("page.jpg", &page, EncodeSettings::default()).expect("encodes");
    let baseline = encode(
        "page.jpg",
        &page,
        EncodeSettings {
            progressive: false,
            ..EncodeSettings::default()
        },
    )
    .expect("encodes");

    assert_eq!(start_of_frame(&progressive), Some(0xC2), "expected SOF2");
    assert_eq!(start_of_frame(&baseline), Some(0xC0), "expected SOF0");
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
fn scaled_decode_picks_the_smallest_sufficient_step() {
    let source = banded(1520, 2150);
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
    let source = banded(1520, 2150);
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
    let source = banded(1520, 2150);
    let resized = Resampler::new()
        .resize("big.jpg", &source, 1280, Filter::default())
        .expect("resizes");
    assert_eq!((resized.width(), resized.height()), (1280, 1811));
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
/// mozjpeg's source manager synthesises an end-of-image marker when the input runs out,
/// so a JPEG truncated after its headers decodes to a partially filled image instead of
/// failing. The Go implementation had the same behaviour, through the same library. This
/// test exists so that the day it changes is a deliberate day.
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
