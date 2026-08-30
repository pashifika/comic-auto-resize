//! JPEG decode, through libjpeg's decoder API.
//!
//! Two libjpeg features decide this: scaled decode, which lets a page that is about to
//! be shrunk skip most of its own IDCT work, and IDCT method selection. No pure-Rust
//! decoder offers either, so the decoder that is already linked for its encoder is used
//! for both directions.

use std::os::raw::c_int;
use std::panic;

use mozjpeg::{ColorSpace, Decompress, Warnings};
use mozjpeg_sys::{
    JWRN_ADOBE_XFORM, JWRN_BOGUS_ICC, JWRN_BOGUS_PROGRESSION, JWRN_EXTRANEOUS_DATA,
    JWRN_HIT_MARKER, JWRN_HUFF_BAD_CODE, JWRN_JFIF_MAJOR, JWRN_JPEG_EOF, JWRN_MUST_RESYNC,
    JWRN_NOT_SEQUENTIAL, JWRN_TOO_MUCH_DATA,
};

use super::{
    Budget, Channels, DctMethod, PageError, PageErrorKind, PageImage, SOI_MARKER, require_soi,
    unwind_reason,
};

/// JPEG's end-of-image marker.
const EOI_MARKER: [u8; 2] = [0xFF, 0xD9];

/// Codes that mean libjpeg substituted data for something damaged and carried on. Refused:
/// the page decoded to full size out of partly fabricated coefficients.
///
/// `JWRN_TOO_MUCH_DATA` is a caller-misuse warning rather than a data one and cannot arise
/// from this decoder; it is listed here so that if it ever did, it would refuse rather than
/// fall through.
const REPAIR_CODES: [c_int; 5] = [
    JWRN_BOGUS_PROGRESSION,
    JWRN_EXTRANEOUS_DATA,
    JWRN_HUFF_BAD_CODE,
    JWRN_MUST_RESYNC,
    JWRN_TOO_MUCH_DATA,
];

/// Codes that describe a non-conforming *header* libjpeg interpreted, not damage it
/// repaired. Ignored, because refusing them would reject files that decode perfectly.
///
/// libjpeg says so itself. Of `JWRN_NOT_SEQUENTIAL`, `jdhuff.c`: "This ought to be an error
/// condition, but we make it a warning because there are some baseline files out there with
/// all zeroes in these bytes." Of `JWRN_JFIF_MAJOR`, `jdmarker.c`: "now it's a nonfatal
/// warning, because some bozo at Hijaak couldn't read the spec." `JWRN_ADOBE_XFORM` makes
/// libjpeg assume YCbCr and decode. `JWRN_BOGUS_ICC` concerns a profile this tool discards
/// anyway.
const BENIGN_CODES: [c_int; 4] = [
    JWRN_ADOBE_XFORM,
    JWRN_BOGUS_ICC,
    JWRN_JFIF_MAJOR,
    JWRN_NOT_SEQUENTIAL,
];

/// Codes a stream truncated after its headers produces, accepted only together with the
/// structural test in [`is_truncated`].
///
/// Measured by decoding the committed fixture truncated at every offset from the start of
/// its scan: every observed set was a subset of these two plus `JWRN_EXTRANEOUS_DATA`, and
/// every one contained `JWRN_JPEG_EOF`.
const TRUNCATION_CODES: [c_int; 2] = [JWRN_HIT_MARKER, JWRN_JPEG_EOF];

/// What the decoder is allowed to do to a page on the way in.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DecodeSettings {
    pub dct_method: DctMethod,
    /// Size the decode may scale down towards, as `(width, height)`.
    ///
    /// `None` decodes at full size. Scaling here is free relative to decoding and then
    /// resampling, but it is coarse — eighths of the original — so the resampler still
    /// does the final step.
    pub scale_to: Option<(u32, u32)>,
    /// What the page is allowed to cost. Checked after the header is read and before
    /// libjpeg allocates for it.
    pub budget: Budget,
}

/// Decodes `buffer` into 8-bit pixels, keeping the source's channel count.
///
/// A grayscale JPEG comes back as [`Channels::Gray`] and everything else as
/// [`Channels::Rgb`]; see [`Channels`] for why the grayscale case is not widened. EXIF and
/// ICC data are not returned — see the module documentation.
///
/// # Errors
///
/// [`PageErrorKind::NotJpeg`] when `buffer` does not begin with the start-of-image marker,
/// [`PageErrorKind::TooLarge`] when the header or the decode target exceeds
/// `settings.budget`, and [`PageErrorKind::Decode`] when libjpeg rejects the stream.
/// libjpeg reports a fatal error by unwinding out of C; that unwind is caught here and
/// returned, so one unreadable page cannot take the process down with it.
pub fn decode(name: &str, buffer: &[u8], settings: DecodeSettings) -> Result<PageImage, PageError> {
    require_soi(name, buffer)?;

    let DecodedPage {
        width,
        height,
        original_width,
        original_height,
        channels,
        pixels,
    } = panic::catch_unwind(|| decode_pixels(buffer, settings))
        .map_err(|payload| {
            PageError::new(name, PageErrorKind::Decode(unwind_reason(payload.as_ref())))
        })?
        .map_err(|kind| PageError::new(name, kind))?;

    PageImage::new(width, height, channels, pixels)
        .map(|page| page.scaled_from(original_width, original_height))
        .map_err(|error| PageError::new(name, error.into()))
}

/// The geometry `buffer`'s header declares, without decoding it.
///
/// The resize policy needs the source geometry before the decode is configured: the target
/// height follows from the source's aspect ratio, and whether the page is resized at all
/// depends on both axes. Reading the header twice costs marker parsing, which is
/// microseconds against a decode, and it keeps the policy above the codec instead of
/// inside it.
///
/// # Errors
///
/// [`PageErrorKind::NotJpeg`] when `buffer` does not begin with the start-of-image marker,
/// and [`PageErrorKind::Decode`] when libjpeg rejects the header.
pub fn header(name: &str, buffer: &[u8]) -> Result<(u32, u32), PageError> {
    require_soi(name, buffer)?;

    panic::catch_unwind(|| {
        let decompress = Decompress::new_mem(buffer)?;
        Ok((
            dimension(decompress.width()),
            dimension(decompress.height()),
        ))
    })
    .map_err(|payload| {
        PageError::new(name, PageErrorKind::Decode(unwind_reason(payload.as_ref())))
    })?
    .map_err(|kind| PageError::new(name, kind))
}

/// One decoded page: the buffer, its own dimensions, and the ones the header declared.
///
/// The two pairs differ whenever `scale` was applied, and they are kept apart because the
/// resampler needs the header's aspect ratio rather than the scaled buffer's.
struct DecodedPage {
    width: u32,
    height: u32,
    original_width: u32,
    original_height: u32,
    channels: Channels,
    pixels: Vec<u8>,
}

/// The part that may unwind. Kept separate so the `catch_unwind` closure stays trivial.
fn decode_pixels(buffer: &[u8], settings: DecodeSettings) -> Result<DecodedPage, PageErrorKind> {
    let mut decompress = Decompress::new_mem(buffer)?;
    decompress.dct_method(settings.dct_method.into());

    // The header's own dimensions, read before `scale` is chosen: `Decompress` reports the
    // size the stream declares, and only `DecompressStarted` reports the scaled output's.
    let original_width = dimension(decompress.width());
    let original_height = dimension(decompress.height());

    // `jpeg_read_header` has run and `jpeg_start_decompress` has not, so this is after the
    // geometry is trustworthy and before anything is allocated from it.
    settings
        .budget
        .allow_source(original_width, original_height)?;

    let numerator = settings.scale_to.and_then(|(target_width, target_height)| {
        scale_numerator(original_width, original_height, target_width, target_height)
    });
    if let Some(numerator) = numerator {
        decompress.scale(numerator);
    }

    // `color_space()` reports the *source* colour space, before any conversion is chosen,
    // which is what decides the request. Asking a grayscale page for `rgb()` would triple
    // every buffer from here to the encoder and grow the output file; the Go
    // implementation branched on the same fact — `num_components == 1` with
    // `jpeg_color_space == JCS_GRAYSCALE` — and returned an `image.Gray`.
    let channels = if matches!(decompress.color_space(), ColorSpace::JCS_GRAYSCALE) {
        Channels::Gray
    } else {
        Channels::Rgb
    };

    // The buffer libjpeg is about to fill, at whichever step was chosen — so a page whose
    // smallest step still exceeds the budget is refused rather than decoded at `1/8`.
    let scaled = scaled_dimension(numerator);
    settings
        .budget
        .allow_image(scaled(original_width), scaled(original_height), channels)?;

    let mut started = match channels {
        Channels::Gray => decompress.grayscale()?,
        Channels::Rgb => decompress.rgb()?,
    };
    // These are the *output* dimensions, which differ from the header's whenever `scale`
    // was applied, and must be read before the handle is consumed.
    let pixels: Vec<u8> = started.read_scanlines()?;
    let width = dimension(started.width());
    let height = dimension(started.height());
    // After finishing, not before: `jpeg_finish_decompress` reads on to the end-of-image
    // marker, and `next_marker` warns there about extraneous data before it, so the set is
    // only complete once that read has happened.
    let warnings = started.finish_with_warnings()?;

    if !is_accepted(warnings, buffer) {
        return Err(PageErrorKind::Repaired {
            codes: warnings.codes().collect(),
        });
    }

    Ok(DecodedPage {
        width,
        height,
        original_width,
        original_height,
        channels,
        pixels,
    })
}

/// Whether libjpeg's report is one this build accepts.
///
/// Every code is classified, and anything unclassified is refused, so a libjpeg that grows a
/// new warning fails closed rather than silently passing.
///
/// - [`REPAIR_CODES`] mean fabricated data. Refused.
/// - [`BENIGN_CODES`] describe a non-conforming header libjpeg interpreted. Ignored. Refusing
///   these was a defect: one such page would have refused the whole archive, because a page
///   failure ends the run, with a message saying libjpeg had repaired damage when it had not.
/// - [`TRUNCATION_CODES`] are accepted only when the stream really is short, which is the
///   recorded parity exception — the Go implementation accepted post-header truncation
///   through the same library.
///
/// # What this does not prove
///
/// It separates *accidental* truncation from *accidental* corruption, and nothing stronger.
/// Measured over every single-byte corruption of the fixture's entropy data that libjpeg
/// accepted — about 105,000 cases — 11 produced `{JWRN_JPEG_EOF}` alone, which is also a set
/// truncation produces. An attacker who corrupts entropy data *and* strips the end-of-image
/// marker satisfies both tests and gets a page re-encoded from partly fabricated pixels.
///
/// That residue is accepted deliberately. Closing it means dropping the truncation exception
/// entirely, and the cost of this bound is a damaged page in an output archive — not memory
/// safety, not a privilege boundary. Requiring `JWRN_JPEG_EOF` and refusing
/// `JWRN_EXTRANEOUS_DATA` narrowed the measured overlap from 266 cases to those 11.
fn is_accepted(warnings: Warnings, buffer: &[u8]) -> bool {
    if warnings.is_empty() {
        return true;
    }
    // A code this build cannot name is a code it cannot classify.
    if warnings.has_unnamed_code() {
        return false;
    }

    let mut truncation_shaped = false;
    for code in warnings.codes() {
        if REPAIR_CODES.contains(&code) {
            return false;
        }
        if BENIGN_CODES.contains(&code) {
            continue;
        }
        if TRUNCATION_CODES.contains(&code) {
            truncation_shaped = true;
            continue;
        }
        // A code no list names. Refused, so a libjpeg that grows a new warning fails closed.
        return false;
    }

    if !truncation_shaped {
        // Only benign codes: the page decoded normally.
        return true;
    }
    // A genuinely short stream always runs out of data, so the EOF code must be there, and
    // the buffer must actually lack its closing marker.
    warnings.contains(JWRN_JPEG_EOF) && is_truncated(buffer)
}

/// Whether the stream ends before its end-of-image marker.
///
/// Searched from the start of the scan rather than across the whole buffer: an EXIF
/// thumbnail is itself a JPEG and carries its own marker, so a whole-buffer search would
/// call a truncated page complete. Within valid entropy data a literal `FF` is stuffed as
/// `FF 00`, so the marker cannot occur there; corruption can synthesise one, which reports
/// "complete" and refuses the page — the safe direction.
fn is_truncated(buffer: &[u8]) -> bool {
    let Some(scan) = scan_offset(buffer) else {
        return false;
    };
    !buffer[scan..].windows(2).any(|pair| pair == EOI_MARKER)
}

/// The offset of the start-of-scan marker, found by walking each segment's declared length.
///
/// Not a byte scan for `FF DA`: a quantisation table forced into the baseline range holds
/// entries of exactly `FF`, so `FF` followed by an arbitrary quantiser byte occurs inside
/// `DQT` and a naive scan stops early.
fn scan_offset(buffer: &[u8]) -> Option<usize> {
    let mut index = SOI_MARKER.len();
    while index + 3 < buffer.len() {
        if buffer[index] != 0xFF {
            return None;
        }
        match buffer[index + 1] {
            // A fill byte is legal between segments.
            0xFF => index += 1,
            0xDA => return Some(index),
            0xD9 => return None,
            // Markers that carry no payload: TEM, the restart markers, and SOI.
            0x01 | 0xD0..=0xD8 => index += 2,
            _ => {
                let length =
                    usize::from(u16::from_be_bytes([buffer[index + 2], buffer[index + 3]]));
                index = index.checked_add(2)?.checked_add(length)?;
            }
        }
    }
    None
}

/// Picks libjpeg's `scale_num` for a decode that is heading for `target_width` ×
/// `target_height`, or `None` to decode at full size.
///
/// Ports `go-libjpeg`'s `setupDecoderOptions`: the smallest numerator whose output stays
/// at or above the target on both axes, where libjpeg's own output size is
/// `ceil(dimension * numerator / 8)`. Only `1..=7` are considered, because `8/8` is
/// what a full-size decode already does — the Go code reaches the same conclusion by
/// searching `1..=8` and then discarding `8`.
///
/// A zero on either target axis returns `None`, matching the reference's `tw > 0 && th > 0`
/// guard. Without it a zero axis would be trivially satisfied by `1/8` and an eighth-size
/// decode would be chosen from one usable target instead of two.
///
/// Undershooting here cannot be repaired later: the resampler would have to upscale
/// pixels the decoder threw away.
#[must_use]
pub fn scale_numerator(
    src_width: u32,
    src_height: u32,
    target_width: u32,
    target_height: u32,
) -> Option<u8> {
    if target_width == 0 || target_height == 0 {
        return None;
    }

    (1..8u8).find(|&numerator| {
        let scaled = scaled_dimension(Some(numerator));
        scaled(src_width) >= target_width && scaled(src_height) >= target_height
    })
}

/// libjpeg's output size for one axis at `numerator`/8, where `None` means a full-size
/// decode.
///
/// `ceil` per axis, independently, which is why a scaled buffer no longer carries the
/// page's aspect ratio. Shared with the budget check so the size it refuses is the size
/// libjpeg would produce.
fn scaled_dimension(numerator: Option<u8>) -> impl Fn(u32) -> u32 {
    let numerator = u64::from(numerator.unwrap_or(8));
    move |dimension| {
        let scaled = (numerator * u64::from(dimension)).div_ceil(8);
        u32::try_from(scaled).unwrap_or(u32::MAX)
    }
}

/// Narrows one of libjpeg's `usize` dimensions.
///
/// A JPEG's dimensions are 16-bit in the format itself, so this cannot lose information
/// on a stream libjpeg accepted. The saturating fallback exists so that a nonsense
/// header becomes a buffer-length mismatch rather than a panic.
fn dimension(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::scale_numerator;
    use super::{BENIGN_CODES, REPAIR_CODES, TRUNCATION_CODES, is_truncated, scan_offset};

    /// Every warning code libjpeg defines is classified exactly once.
    ///
    /// The lists are what decides whether a page is refused, so a code appearing twice, or in
    /// none of them, is a policy hole rather than an untidy constant. An unlisted code is
    /// refused at runtime, which is the safe direction, but silently — this makes it loud.
    #[test]
    fn the_three_classifications_are_disjoint_and_total() {
        // `mozjpeg_sys`' table, codes 114 to 128. `JWRN_ARITH_BAD_CODE` is absent from this
        // build's table; were it present it would be refused as unlisted.
        let known = [
            mozjpeg_sys::JWRN_ADOBE_XFORM,
            mozjpeg_sys::JWRN_BOGUS_ICC,
            mozjpeg_sys::JWRN_BOGUS_PROGRESSION,
            mozjpeg_sys::JWRN_EXTRANEOUS_DATA,
            mozjpeg_sys::JWRN_HIT_MARKER,
            mozjpeg_sys::JWRN_HUFF_BAD_CODE,
            mozjpeg_sys::JWRN_JFIF_MAJOR,
            mozjpeg_sys::JWRN_JPEG_EOF,
            mozjpeg_sys::JWRN_MUST_RESYNC,
            mozjpeg_sys::JWRN_NOT_SEQUENTIAL,
            mozjpeg_sys::JWRN_TOO_MUCH_DATA,
        ];

        for code in known {
            let listed = usize::from(REPAIR_CODES.contains(&code))
                + usize::from(BENIGN_CODES.contains(&code))
                + usize::from(TRUNCATION_CODES.contains(&code));
            assert_eq!(
                listed, 1,
                "warning code {code} is classified {listed} times"
            );
        }

        assert_eq!(
            REPAIR_CODES.len() + BENIGN_CODES.len() + TRUNCATION_CODES.len(),
            known.len(),
            "a list holds a code that is not one of libjpeg's warnings"
        );
    }

    #[test]
    fn truncation_is_decided_from_the_scan_onwards() {
        // A complete stream: header, scan, entropy byte, closing marker.
        let complete = [0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02, 0x42, 0xFF, 0xD9];
        assert_eq!(scan_offset(&complete), Some(2));
        assert!(!is_truncated(&complete));

        // The same stream with its closing marker cut off.
        assert!(is_truncated(&complete[..complete.len() - 2]));

        // A closing marker that belongs to a thumbnail *before* the scan must not count as
        // the page's own, or a truncated page would read as complete.
        let thumbnail_eoi = [
            0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x04, 0xFF, 0xD9, 0xFF, 0xDA, 0x00, 0x02, 0x42,
        ];
        assert_eq!(scan_offset(&thumbnail_eoi), Some(8));
        assert!(is_truncated(&thumbnail_eoi));
    }

    #[test]
    fn picks_the_smallest_numerator_that_clears_the_target() {
        // 6/8 of 1520 is 1140, which is below 1280; 7/8 is 1330, which is not.
        assert_eq!(scale_numerator(1520, 2150, 1280, 1811), Some(7));
    }

    #[test]
    fn skips_scaling_when_the_target_is_not_smaller() {
        assert_eq!(scale_numerator(1520, 2150, 1520, 2150), None);
        assert_eq!(scale_numerator(1000, 1400, 2000, 2800), None);
    }

    #[test]
    fn both_axes_have_to_clear_the_target() {
        // 4/8 clears the width but not the height, so the height decides.
        assert_eq!(scale_numerator(1600, 2400, 800, 1800), Some(6));
    }

    #[test]
    fn output_size_is_rounded_up_like_libjpeg() {
        // ceil(3 * 1/8) is 1, so a 3-pixel axis still satisfies a 1-pixel target at 1/8.
        assert_eq!(scale_numerator(3, 3, 1, 1), Some(1));
    }

    #[test]
    fn a_zero_target_axis_decodes_at_full_size() {
        // Without the reference's `tw > 0 && th > 0` guard, a zero axis is satisfied by
        // every numerator and `1/8` wins — an eighth-size decode chosen from half a
        // target. Both orders, so the guard cannot be half-applied.
        assert_eq!(scale_numerator(1520, 2150, 0, 1811), None);
        assert_eq!(scale_numerator(1520, 2150, 1280, 0), None);
        assert_eq!(scale_numerator(1520, 2150, 0, 0), None);
    }
}
