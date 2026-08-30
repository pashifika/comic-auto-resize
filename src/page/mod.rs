//! Decode, resize, and encode one comic page.
//!
//! JPEG is the only format here. It is what manga archives contain, and mozjpeg — used
//! for both directions, because scaled decode and IDCT selection need libjpeg's decoder
//! API — is the whole reason the rewrite can shrink an archive without visibly hurting
//! line art. png, bmp, and webp arrive later.
//!
//! Every entry point takes the name of the page it is working on. libjpeg reports
//! failures by unwinding, and a page that fails must be attributable without the caller
//! having to remember which one it had passed in.
//!
//! **Metadata does not survive a page.** The decoder is asked for pixels only, so EXIF
//! (including the orientation tag) and any ICC profile are read past and dropped, and the
//! encoder writes no `APPn` marker of its own beyond the `JFIF` header libjpeg emits. A
//! page whose EXIF asked a viewer to rotate it therefore comes out unrotated, and a page
//! in a wide-gamut profile comes out untagged. The Go implementation dropped both the same
//! way through the same library, so this is parity rather than a regression; it is recorded
//! here because nothing in the signatures says so.

mod budget;
mod decode;
mod encode;
mod resize;

pub use budget::Budget;
pub use decode::{DecodeSettings, decode, scale_numerator};
pub use encode::{EncodeSettings, encode};
pub use resize::{Filter, Resampler, UnknownFilter, height_for_width};

use std::any::Any;
use std::fmt;
use std::io;

use thiserror::Error;

/// JPEG's start-of-image marker, which every JPEG stream begins with.
pub const SOI_MARKER: [u8; 2] = [0xFF, 0xD8];

/// How many 8-bit samples one pixel of a page carries.
///
/// A single-component source stays single-component from decode through to re-encode.
/// Expanding grayscale to RGB triples every buffer and makes the *output* larger too.
/// Measured on a 1280x1800 banded probe at the default settings: 2,304,000 bytes decoded
/// as one channel against 6,912,000 as three, re-encoding to 74,103 bytes against 76,700.
/// Growing the file is backwards for a tool whose purpose is shrinking an archive, and the
/// Go implementation preserved `image.Gray` end to end for the same reason.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Channels {
    /// One sample per pixel: a grayscale page.
    Gray,
    /// Three samples per pixel, red then green then blue.
    #[default]
    Rgb,
}

impl Channels {
    /// Samples per pixel, which for 8-bit data is also bytes per pixel.
    #[must_use]
    pub const fn count(self) -> u32 {
        match self {
            Self::Gray => 1,
            Self::Rgb => 3,
        }
    }
}

impl fmt::Display for Channels {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Gray => "grayscale",
            Self::Rgb => "RGB",
        })
    }
}

/// The algorithm libjpeg uses for the DCT and IDCT steps.
///
/// The names are the ones the tool's `--dct` flag accepts, kept in this crate so the
/// flag's vocabulary does not become whatever the binding happens to call it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DctMethod {
    /// Floating-point: accurate, and fast where floating point is fast.
    Float,
    /// Faster, less accurate integer method. The default, matching the Go
    /// implementation's `--dct ifast`.
    #[default]
    IntegerFast,
    /// Slow but accurate integer algorithm.
    IntegerSlow,
}

impl From<DctMethod> for mozjpeg::DctMethod {
    fn from(method: DctMethod) -> Self {
        match method {
            DctMethod::Float => Self::Float,
            DctMethod::IntegerFast => Self::IntegerFast,
            DctMethod::IntegerSlow => Self::IntegerSlow,
        }
    }
}

/// An 8-bit image held as a single tightly packed buffer.
///
/// The buffer length is checked once, in [`PageImage::new`], so every consumer can index
/// it by `(y * width + x) * channels` without re-deriving whether that is in bounds.
///
/// It also carries the geometry the page declared in its *header*, which is larger than
/// the buffer whenever the buffer came out of a scaled decode. libjpeg rounds each axis of
/// a scaled decode up on its own, so such a buffer no longer carries the page's aspect
/// ratio and the resampler needs the page's, not the buffer's. Only the decoder knows a
/// geometry other than the buffer's own, and only the decoder can record one:
/// [`PageImage::new`] records the buffer's dimensions and [`PageImage::scaled_from`] is
/// crate-private, so nothing outside this crate can claim a page larger than the pixels it
/// is holding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageImage {
    width: u32,
    height: u32,
    /// The page's geometry before any scaled decode, equal to `width` × `height` unless
    /// [`PageImage::scaled_from`] recorded otherwise.
    original_width: u32,
    original_height: u32,
    channels: Channels,
    pixels: Vec<u8>,
}

impl PageImage {
    /// Wraps `pixels` as a `width` × `height` image of `channels` samples per pixel.
    ///
    /// The page's original geometry is recorded as `width` × `height`: a buffer handed in
    /// from outside is the whole page, not a scaled view of a larger one.
    ///
    /// # Errors
    ///
    /// [`InvalidPixelBuffer`] when `pixels` is not exactly
    /// `width * height * channels.count()` bytes.
    pub fn new(
        width: u32,
        height: u32,
        channels: Channels,
        pixels: Vec<u8>,
    ) -> Result<Self, InvalidPixelBuffer> {
        // Widened to `u128` rather than multiplied in `usize`: `u32::MAX` on both axes
        // times three channels overflows `u64` as well, and a release build has overflow
        // checks off, so a wrapped product would accept a small buffer against huge
        // dimensions — the one invariant this type exists to establish. A length that does
        // not fit `usize` cannot equal `pixels.len()`, which is what `try_from` expresses.
        let expected = u128::from(width) * u128::from(height) * u128::from(channels.count());
        if usize::try_from(expected).is_ok_and(|expected| expected == pixels.len()) {
            Ok(Self {
                width,
                height,
                original_width: width,
                original_height: height,
                channels,
                pixels,
            })
        } else {
            Err(InvalidPixelBuffer {
                width,
                height,
                channels,
                expected,
                actual: pixels.len(),
            })
        }
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The width the page declared in its header.
    ///
    /// Equal to [`PageImage::width`] unless this buffer is a scaled decode of a larger
    /// page, in which case it is the width the aspect ratio has to be taken from.
    #[must_use]
    pub const fn original_width(&self) -> u32 {
        self.original_width
    }

    /// The height the page declared in its header.
    ///
    /// Equal to [`PageImage::height`] unless this buffer is a scaled decode of a larger
    /// page, in which case it is the height the aspect ratio has to be taken from.
    #[must_use]
    pub const fn original_height(&self) -> u32 {
        self.original_height
    }

    #[must_use]
    pub const fn channels(&self) -> Channels {
        self.channels
    }

    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Records that this buffer is a scaled decode of an `original_width` ×
    /// `original_height` page.
    ///
    /// Crate-private on purpose. The decoder is the only place a page's real geometry is
    /// known, and an original larger than the buffer is the only thing that lets
    /// [`Resampler::resize`] derive a destination out of proportion to the buffer it was
    /// handed. Keeping this out of the public API is what bounds that destination for
    /// every caller outside this crate; [`Resampler::resize`] checks the recorded
    /// geometry against the buffer for callers inside it.
    #[must_use]
    pub(crate) fn scaled_from(mut self, original_width: u32, original_height: u32) -> Self {
        self.original_width = original_width;
        self.original_height = original_height;
        self
    }
}

/// A pixel buffer whose length disagrees with the dimensions it was given.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("expected {expected} bytes for a {width}x{height} {channels} image, got {actual}")]
pub struct InvalidPixelBuffer {
    pub width: u32,
    pub height: u32,
    pub channels: Channels,
    /// Held as `u128` because the product of two `u32` axes and the channel count can
    /// exceed every narrower type, and a truncated number in the message would misreport
    /// the very mismatch it exists to explain.
    pub expected: u128,
    pub actual: usize,
}

/// A page that could not be processed, and which page it was.
#[derive(Debug, Error)]
#[error("{name}: {kind}")]
pub struct PageError {
    /// The name the caller passed in — an archive entry path, in the finished pipeline.
    pub name: String,
    pub kind: PageErrorKind,
}

impl PageError {
    fn new(name: &str, kind: PageErrorKind) -> Self {
        Self {
            name: name.to_owned(),
            kind,
        }
    }
}

/// What went wrong with a page, independent of which page it was.
///
/// Separate from [`PageError`] so the pipeline can decide what to do about a failure —
/// skip the page, or give up on the archive — by matching on the cause alone.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PageErrorKind {
    /// Rejected before libjpeg was invoked at all.
    #[error("not a JPEG: expected the FF D8 start-of-image marker, found {0}")]
    NotJpeg(String),
    #[error("JPEG decode failed: {0}")]
    Decode(String),
    #[error("resize failed: {0}")]
    Resize(String),
    #[error("JPEG encode failed: {0}")]
    Encode(String),
    /// A quality outside libjpeg's scale, rejected rather than silently clamped by it.
    #[error("quality must be 1 to 100, got {0}")]
    Quality(u8),
    /// A buffer handed between the two libraries did not match its own dimensions.
    #[error("inconsistent pixel buffer: {0}")]
    Buffer(#[from] InvalidPixelBuffer),
    /// Refused before allocating for it, because an input-controlled size exceeded an
    /// internal limit. See [`Budget`].
    #[error("{quantity} is {actual}, over the limit of {limit}")]
    TooLarge {
        quantity: &'static str,
        actual: u64,
        limit: u64,
    },
    /// libjpeg found the data damaged, substituted coefficients for it, and reported
    /// success. Refused rather than re-encoded, because the pixels are partly fabricated.
    #[error("libjpeg repaired damaged data and reported success (warning codes {codes:?})")]
    Repaired { codes: Vec<i32> },
}

impl From<io::Error> for PageErrorKind {
    /// Both native libraries report a refusal as an `io::Error`, and every such refusal
    /// reaching this crate came out of a decode.
    fn from(error: io::Error) -> Self {
        Self::Decode(error.to_string())
    }
}

/// Rejects a buffer that cannot be a JPEG before libjpeg is handed it.
///
/// libjpeg's own rejection arrives as an unwind out of C, which is recoverable but
/// needlessly expensive for a page that is obviously a PNG.
fn require_soi(name: &str, buffer: &[u8]) -> Result<(), PageError> {
    if buffer.starts_with(&SOI_MARKER) {
        return Ok(());
    }

    let found = if buffer.is_empty() {
        "an empty buffer".to_owned()
    } else {
        buffer
            .iter()
            .take(SOI_MARKER.len())
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    Err(PageError::new(name, PageErrorKind::NotJpeg(found)))
}

/// Turns a caught unwind back into a message.
///
/// mozjpeg's error handler resumes unwinding with a `String` describing libjpeg's
/// complaint, so the useful text is in the payload rather than on stderr.
fn unwind_reason(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return (*message).to_owned();
    }
    "libjpeg unwound without a message".to_owned()
}

#[cfg(test)]
mod tests {
    use super::{Channels, InvalidPixelBuffer, PageImage};

    #[test]
    fn a_buffer_matching_its_dimensions_is_accepted() {
        let gray = PageImage::new(2, 3, Channels::Gray, vec![0; 6]).expect("2 * 3 * 1");
        assert_eq!(gray.channels(), Channels::Gray);
        assert_eq!(gray.pixels().len(), 6);

        let rgb = PageImage::new(2, 3, Channels::Rgb, vec![0; 18]).expect("2 * 3 * 3");
        assert_eq!(rgb.channels(), Channels::Rgb);
        assert_eq!(rgb.pixels().len(), 18);
    }

    #[test]
    fn the_channel_count_is_part_of_the_expected_length() {
        // The same buffer that fits a grayscale image cannot fit an RGB one.
        let error = PageImage::new(2, 3, Channels::Rgb, vec![0; 6]).expect_err("6 != 18");
        assert!(matches!(error, InvalidPixelBuffer { expected: 18, .. }));
        assert!(
            error.to_string().contains("RGB"),
            "the message must name the layout: {error}"
        );
    }

    #[test]
    fn huge_dimensions_do_not_wrap_into_a_matching_length() {
        // 65536 x 65536 x 3 is 12,884,901,888 bytes. That is 0x3_0000_0000, so on a
        // 32-bit `usize` the old `width as usize * height as usize * RGB_CHANNELS`
        // wrapped to exactly 0 and accepted an empty buffer against a four-gigapixel
        // image. The `u128` product cannot wrap on any target.
        let error = PageImage::new(65536, 65536, Channels::Rgb, Vec::new())
            .expect_err("an empty buffer is not four gigapixels of RGB");
        assert_eq!(error.expected, 12_884_901_888);
        assert_eq!(error.actual, 0);

        // Beyond `u64` as well: 4294967295 x 4294967295 x 3 is 55340232195358851075,
        // just under three times `u64::MAX`, so it cannot be computed in `usize` on any
        // target Rust supports.
        let error = PageImage::new(u32::MAX, u32::MAX, Channels::Rgb, Vec::new())
            .expect_err("no buffer is u32::MAX squared pixels of RGB");
        assert_eq!(error.expected, 55_340_232_195_358_851_075);
    }
}
