//! JPEG decode, through libjpeg's decoder API.
//!
//! Two libjpeg features decide this: scaled decode, which lets a page that is about to
//! be shrunk skip most of its own IDCT work, and IDCT method selection. No pure-Rust
//! decoder offers either, so the decoder that is already linked for its encoder is used
//! for both directions.

use std::io;
use std::panic;

use mozjpeg::{ColorSpace, Decompress};

use super::{Channels, DctMethod, PageError, PageErrorKind, PageImage, require_soi, unwind_reason};

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
}

/// Decodes `buffer` into 8-bit pixels, keeping the source's channel count.
///
/// A grayscale JPEG comes back as [`Channels::Gray`] and everything else as
/// [`Channels::Rgb`]; see [`Channels`] for why the grayscale case is not widened. EXIF and
/// ICC data are not returned — see the module documentation.
///
/// # Errors
///
/// [`PageErrorKind::NotJpeg`] when `buffer` does not begin with the start-of-image
/// marker, and [`PageErrorKind::Decode`] when libjpeg rejects the stream. libjpeg
/// reports a fatal error by unwinding out of C; that unwind is caught here and returned,
/// so one unreadable page cannot take the process down with it.
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
        .map_err(|error| PageError::new(name, PageErrorKind::Decode(error.to_string())))?;

    PageImage::new(width, height, channels, pixels)
        .map(|page| page.scaled_from(original_width, original_height))
        .map_err(|error| PageError::new(name, error.into()))
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
fn decode_pixels(buffer: &[u8], settings: DecodeSettings) -> io::Result<DecodedPage> {
    let mut decompress = Decompress::new_mem(buffer)?;
    decompress.dct_method(settings.dct_method.into());

    // The header's own dimensions, read before `scale` is chosen: `Decompress` reports the
    // size the stream declares, and only `DecompressStarted` reports the scaled output's.
    let original_width = dimension(decompress.width());
    let original_height = dimension(decompress.height());

    if let Some((target_width, target_height)) = settings.scale_to {
        let numerator =
            scale_numerator(original_width, original_height, target_width, target_height);
        if let Some(numerator) = numerator {
            decompress.scale(numerator);
        }
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
    let mut started = match channels {
        Channels::Gray => decompress.grayscale()?,
        Channels::Rgb => decompress.rgb()?,
    };
    // Read before `finish`, which consumes the handle. These are the *output* dimensions,
    // which differ from the header's whenever `scale` was applied.
    let pixels: Vec<u8> = started.read_scanlines()?;
    let width = dimension(started.width());
    let height = dimension(started.height());
    started.finish()?;

    Ok(DecodedPage {
        width,
        height,
        original_width,
        original_height,
        channels,
        pixels,
    })
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
        let scaled = |dimension: u32| (u64::from(numerator) * u64::from(dimension)).div_ceil(8);
        scaled(src_width) >= u64::from(target_width)
            && scaled(src_height) >= u64::from(target_height)
    })
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
