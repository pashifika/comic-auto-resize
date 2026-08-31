//! Decoding one page, behind the format its bytes selected.
//!
//! One encoder, many decoders — the reference implementation's shape
//! (`utils/images/images.go:60-108`) and this project's rule. The dispatch is the whole of
//! this module; each decoder is its own file, because [`jpeg`] links a C library and
//! [`raster`] is three formats through one Rust crate.
//!
//! # The two decoders do not offer the same thing, and that is stated rather than hidden
//!
//! JPEG's decode can be *scaled*: the header's dimensions are read, a `scale_denom` is
//! chosen, and a page four times the target width never exists at full size. png, bmp and
//! webp have no equivalent, so their pixel buffer is a function of the **source** dimensions
//! where JPEG's is a function of the target. The consequence for peak memory is real and
//! belongs in the reader's view of this module, not in a commit message.
//!
//! What both do offer is the two-phase read: [`header`] establishes the declared dimensions
//! without decoding, so the budget refusal happens before a pixel buffer is allocated for
//! every format rather than only where choosing a scale made it incidental.

mod jpeg;
mod raster;

pub use jpeg::scale_numerator;

use super::{Budget, DctMethod, Format, PageError, PageImage};

/// What the decoder is allowed to do to a page on the way in.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DecodeSettings {
    pub dct_method: DctMethod,
    /// Size the decode may scale down towards, as `(width, height)`.
    ///
    /// `None` decodes at full size. Scaling here is free relative to decoding and then
    /// resampling, but it is coarse — eighths of the original — so the resampler still
    /// does the final step. **JPEG only**: no other decoder here can scale, and for those
    /// this is ignored rather than approximated.
    pub scale_to: Option<(u32, u32)>,
    /// What the page is allowed to cost. Checked after the header is read and before
    /// anything allocates for it.
    pub budget: Budget,
}

/// One decoded page, and what had to be done to it to make it a page.
///
/// The flag is here rather than on [`PageImage`] because it is provenance and not pixels:
/// nothing downstream of the decoder reads it except the run's own tally.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Decoded {
    pub page: PageImage,
    /// Whether an alpha channel was composited onto white. JPEG has no alpha, so this is
    /// always `false` for a JPEG page; the run counts the rest and reports the total once.
    /// See `pipeline::Report::composited`.
    pub composited: bool,
}

/// Decodes `buffer` as `format`, into 8-bit pixels the encoder can carry.
///
/// A single-component source stays single-component; see [`Channels`](super::Channels). A
/// channel the encoder cannot carry is resolved by a stated rule rather than dropped: an
/// alpha channel is composited onto white and reported, a sample deeper than eight bits is
/// narrowed and not. EXIF and ICC data are not returned — see the `page` module docs.
///
/// # Errors
///
/// [`PageErrorKind::NotJpeg`](super::PageErrorKind::NotJpeg) when a JPEG buffer does not
/// begin with the start-of-image marker,
/// [`PageErrorKind::TooLarge`](super::PageErrorKind::TooLarge) when the declared dimensions
/// or the buffer the decode would allocate exceed `settings.budget`,
/// [`PageErrorKind::MultiFrame`](super::PageErrorKind::MultiFrame) for an animation,
/// [`PageErrorKind::Pixels`](super::PageErrorKind::Pixels) for a pixel shape no narrowing
/// rule covers, and [`PageErrorKind::Decode`](super::PageErrorKind::Decode) when the decoder
/// rejects the stream — including a sub-format this build cannot read, whose reason names
/// the feature.
pub fn decode(
    name: &str,
    buffer: &[u8],
    format: Format,
    settings: DecodeSettings,
) -> Result<Decoded, PageError> {
    match format {
        Format::Jpeg => jpeg::decode(name, buffer, settings).map(|page| Decoded {
            page,
            // JPEG has no alpha channel to composite, which is the whole reason the other
            // three need a rule for one.
            composited: false,
        }),
        Format::Png => raster::png(name, buffer, settings.budget),
        Format::Bmp => raster::bmp(name, buffer, settings.budget),
        Format::WebP => raster::webp(name, buffer, settings.budget),
    }
}

/// The geometry `buffer`'s header declares, without decoding it.
///
/// The resize policy needs the source geometry before the decode is configured, and the
/// budget needs it before anything is allocated from it. Reading the header twice costs
/// header parsing, which is microseconds against a decode, and it keeps the policy above the
/// codec instead of inside it.
///
/// # Errors
///
/// [`PageErrorKind::NotJpeg`](super::PageErrorKind::NotJpeg) when a JPEG buffer does not
/// begin with the start-of-image marker, and
/// [`PageErrorKind::Decode`](super::PageErrorKind::Decode) when the decoder rejects the
/// header.
pub fn header(name: &str, buffer: &[u8], format: Format) -> Result<(u32, u32), PageError> {
    match format {
        Format::Jpeg => jpeg::header(name, buffer),
        Format::Png => raster::png_header(name, buffer),
        Format::Bmp => raster::bmp_header(name, buffer),
        Format::WebP => raster::webp_header(name, buffer),
    }
}
