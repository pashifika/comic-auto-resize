//! JPEG encode, through mozjpeg's compressor.
//!
//! This is the half that earns the dependency: trellis quantisation, scan optimisation,
//! and mozjpeg's tuned quantisation tables produce a smaller file than libjpeg-turbo at
//! the same visual quality. Quality therefore stays high and the size reduction comes
//! from the resize, not from quantising line art into ringing.

use std::io;
use std::ops::RangeInclusive;
use std::panic;

use mozjpeg::{ColorSpace, Compress, qtable};

use super::{Channels, DctMethod, PageError, PageErrorKind, PageImage, unwind_reason};

/// The quality values libjpeg's scaling curve is defined over.
///
/// libjpeg clamps anything else — 0 becomes 1 and 200 becomes 100 — without telling the
/// caller, so [`encode`] rejects it instead.
const QUALITY: RangeInclusive<u8> = 1..=100;

/// The four encoder settings the tool exposes, matching the Go implementation's flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncodeSettings {
    /// libjpeg quality, 1 to 100. Outside that range [`encode`] fails the page rather
    /// than letting libjpeg clamp it silently.
    pub quality: u8,
    /// Optimise the entropy-coding tables. Costs a pass, saves bytes.
    pub optimize_coding: bool,
    /// Emit a progressive rather than a baseline file.
    pub progressive: bool,
    pub dct_method: DctMethod,
}

impl Default for EncodeSettings {
    /// Quality 90 with optimisation and progressive on, as the Go implementation
    /// defaults.
    fn default() -> Self {
        Self {
            quality: 90,
            optimize_coding: true,
            progressive: true,
            dct_method: DctMethod::default(),
        }
    }
}

/// Encodes `source` and returns the JPEG bytes.
///
/// Returns bytes rather than writing to a path: the pipeline hands them to an ordering
/// writer that puts them into the output archive, and never touches the filesystem for
/// an intermediate.
///
/// A [`Channels::Gray`] source is written as a single-component JPEG and a
/// [`Channels::Rgb`] one as three. No EXIF or ICC data is written — see the module
/// documentation.
///
/// # Errors
///
/// [`PageErrorKind::Quality`] when `settings.quality` is outside 1 to 100, checked before
/// libjpeg is called. [`PageErrorKind::Encode`] when libjpeg refuses the image — a
/// zero-sized one, for instance. As with decoding, libjpeg reports a fatal error by
/// unwinding out of C, and that unwind is caught here.
pub fn encode(
    name: &str,
    source: &PageImage,
    settings: EncodeSettings,
) -> Result<Vec<u8>, PageError> {
    if !QUALITY.contains(&settings.quality) {
        return Err(PageError::new(
            name,
            PageErrorKind::Quality(settings.quality),
        ));
    }

    panic::catch_unwind(|| encode_jpeg(source, settings))
        .map_err(|payload| {
            PageError::new(name, PageErrorKind::Encode(unwind_reason(payload.as_ref())))
        })?
        .map_err(|error| PageError::new(name, PageErrorKind::Encode(error.to_string())))
}

/// The part that may unwind. Kept separate so the `catch_unwind` closure stays trivial.
fn encode_jpeg(source: &PageImage, settings: EncodeSettings) -> io::Result<Vec<u8>> {
    let mut compress = Compress::new(match source.channels() {
        Channels::Gray => ColorSpace::JCS_GRAYSCALE,
        Channels::Rgb => ColorSpace::JCS_RGB,
    });
    compress.set_size(source.width() as usize, source.height() as usize);

    // Quantisation first: installing tables replaces whatever is there, and the settings
    // below must not be overwritten by it.
    //
    // Not `Compress::set_quality`, which is `jpeg_set_quality(cinfo, q, FALSE)` — that
    // last argument is libjpeg's `force_baseline`. With it false, the scaled table keeps
    // entries above 255 at lower qualities, libjpeg writes a 16-bit `DQT`, and
    // `write_frame_header` must then emit `SOF1` (extended sequential) instead of `SOF0`.
    // Baseline compatibility is the only reason to turn `progressive` off, so a baseline
    // switch that yields `SOF1` does not work at all. `set_luma_qtable` and
    // `set_chroma_qtable` go through `jpeg_add_quant_table(..., force_baseline = TRUE)`,
    // which clamps every entry to 1..=255. `go-libjpeg` passed `TRUE` unconditionally as
    // well, so forcing it on both branches is also what the Go implementation shipped.
    //
    // Measured over all 100 qualities on the committed fixture with `progressive: false`:
    // `set_quality` emitted `FFC1` at every quality from 1 to 69 and `FFC0` from 70 up — 70
    // is where the base table's largest entry, 418, first scales below 256. This path
    // emits `FFC0` at all 100, and is byte-identical to `set_quality`'s output at every
    // quality from 70 to 100 except 79. So the only files it changes are the ones that
    // were not baseline to begin with, plus that one quality.
    //
    // Quality 79 is `QTable::scaled` computing the scale factor in `f32`: `75 * 0.42`
    // lands at 31.4999990 rather than 31.5, so it rounds to 31 where libjpeg's integer
    // `(75 * 42 + 50) / 100` gives 32. Two of sixty-four luma entries, one quantiser step.
    // It is not avoidable from here — `QTable`'s coefficients are `pub(crate)`, so
    // `scaled` is the only way to obtain one.
    //
    // `qtable::NRobidoux` is not a preference: mozjpeg's default compression profile is
    // `JCP_MAX_COMPRESSION`, which sets `quant_tbl_master_idx = 3`, and index 3 of both
    // `std_luminance_quant_tbl` and `std_chrominance_quant_tbl` is that table.
    // `QTable::scaled` applies libjpeg's own quality curve to it. Nothing in the binding's
    // types ties the two together, so
    // `the_named_base_table_is_the_one_set_quality_would_install` pins it.
    let table = qtable::NRobidoux.scaled(f32::from(settings.quality), f32::from(settings.quality));
    compress.set_luma_qtable(&table);
    compress.set_chroma_qtable(&table);

    compress.set_optimize_coding(settings.optimize_coding);

    // Not `set_progressive_mode()`. mozjpeg's compression profile already emits a
    // progressive file, built from its own optimised scan script, and that call — libjpeg's
    // `jpeg_simple_progression` — can only replace the script, never remove it. Measured on
    // the committed fixture: doing nothing and calling it both give 1807 bytes with SOF2,
    // while clearing the script gives 1846 bytes with SOF0. The script is therefore what
    // gets toggled, in both directions rather than only one, so the setting does not depend
    // on the library's default staying what it is today.
    compress.set_optimize_scans(settings.progressive);
    // Reachable only through the fork; see `[patch.crates-io]` in `Cargo.toml`.
    compress.set_dct_method(settings.dct_method.into());

    let mut started = compress.start_compress(Vec::new())?;
    started.write_scanlines(source.pixels())?;
    started.finish()
}

#[cfg(test)]
mod tests {
    use std::panic;

    use mozjpeg::{ColorSpace, Compress};

    use super::{EncodeSettings, encode};
    use crate::page::{Channels, DctMethod, PageErrorKind, PageImage};

    const PROBE_SIDE: u32 = 32;

    /// A flat mid-grey probe. Content is irrelevant to the table comparison; the size is
    /// four MCUs on a side so subsampling has something to do.
    fn probe() -> PageImage {
        let side = PROBE_SIDE as usize;
        PageImage::new(
            PROBE_SIDE,
            PROBE_SIDE,
            Channels::Rgb,
            vec![128; side * side * 3],
        )
        .expect("the buffer is built from the dimensions")
    }

    #[test]
    fn defaults_match_the_go_implementation() {
        let settings = EncodeSettings::default();
        assert_eq!(settings.quality, 90);
        assert!(settings.optimize_coding);
        assert!(settings.progressive);
        assert_eq!(settings.dct_method, DctMethod::IntegerFast);
    }

    #[test]
    fn a_quality_outside_libjpegs_scale_is_rejected() {
        for quality in [0, 101, u8::MAX] {
            let error = encode(
                "probe.jpg",
                &probe(),
                EncodeSettings {
                    quality,
                    ..EncodeSettings::default()
                },
            )
            .expect_err("quality outside 1..=100 must not be silently clamped");
            assert!(
                matches!(error.kind, PageErrorKind::Quality(reported) if reported == quality),
                "{quality} reported as {:?}",
                error.kind
            );
        }

        for quality in [1, 50, 100] {
            assert!(
                encode(
                    "probe.jpg",
                    &probe(),
                    EncodeSettings {
                        quality,
                        ..EncodeSettings::default()
                    },
                )
                .is_ok(),
                "quality {quality} is inside the scale and must encode"
            );
        }
    }

    /// Pins the base quantisation table this module names to the one mozjpeg's own
    /// `set_quality` installs.
    ///
    /// `set_quality` cannot be called instead — it passes `force_baseline = FALSE`, which
    /// is the defect this module works around — so the base table has to be named
    /// explicitly, and nothing in the binding's types says `qtable::NRobidoux` is the table
    /// mozjpeg's default compression profile selects. At quality 90 the baseline clamp
    /// changes no entry, so the two paths must agree byte for byte; if mozjpeg ever changes
    /// its default base table, this is what fails.
    #[test]
    fn the_named_base_table_is_the_one_set_quality_would_install() {
        let source = probe();
        let settings = EncodeSettings::default();

        let named = encode("probe.jpg", &source, settings).expect("encodes");

        let side = PROBE_SIDE as usize;
        let through_set_quality = panic::catch_unwind(|| {
            let mut compress = Compress::new(ColorSpace::JCS_RGB);
            compress.set_size(side, side);
            compress.set_quality(f32::from(settings.quality));
            compress.set_optimize_coding(settings.optimize_coding);
            compress.set_optimize_scans(settings.progressive);
            compress.set_dct_method(settings.dct_method.into());
            let mut started = compress.start_compress(Vec::new()).expect("starts");
            started.write_scanlines(source.pixels()).expect("writes");
            started.finish().expect("finishes")
        })
        .expect("libjpeg does not reject a flat 32x32 probe");

        assert_eq!(
            named, through_set_quality,
            "qtable::NRobidoux is no longer the base table mozjpeg's default profile uses"
        );
    }
}
