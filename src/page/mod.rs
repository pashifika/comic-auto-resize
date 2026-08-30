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

mod decode;
mod encode;
mod resize;

pub use decode::{DecodeSettings, decode, scale_numerator};
pub use encode::{EncodeSettings, encode};
pub use resize::{Filter, Resampler, UnknownFilter, height_for_width};

use std::any::Any;

use thiserror::Error;

/// Bytes per pixel in the buffers this module passes around.
///
/// Everything is decoded to 8-bit RGB: `fast_image_resize` convolves `U8x3` with SIMD,
/// and mozjpeg converts to and from YCbCr itself.
pub const RGB_CHANNELS: usize = 3;

/// JPEG's start-of-image marker, which every JPEG stream begins with.
pub const SOI_MARKER: [u8; 2] = [0xFF, 0xD8];

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

/// An 8-bit RGB image held as a single tightly packed buffer.
///
/// The buffer length is checked once, in [`RgbImage::new`], so every consumer can index
/// it by `y * width * 3 + x * 3` without re-deriving whether that is in bounds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RgbImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl RgbImage {
    /// Wraps `pixels` as a `width` × `height` RGB image.
    ///
    /// # Errors
    ///
    /// [`InvalidRgbBuffer`] when `pixels` is not exactly `width * height * 3` bytes.
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self, InvalidRgbBuffer> {
        let expected = width as usize * height as usize * RGB_CHANNELS;
        if pixels.len() == expected {
            Ok(Self {
                width,
                height,
                pixels,
            })
        } else {
            Err(InvalidRgbBuffer {
                width,
                height,
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

    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Consumes the image and returns its buffer, so a re-encode does not copy it.
    #[must_use]
    pub fn into_pixels(self) -> Vec<u8> {
        self.pixels
    }
}

/// A pixel buffer whose length disagrees with the dimensions it was given.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("expected {expected} bytes for a {width}x{height} RGB image, got {actual}")]
pub struct InvalidRgbBuffer {
    pub width: u32,
    pub height: u32,
    pub expected: usize,
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
    /// A buffer handed between the two libraries did not match its own dimensions.
    #[error("inconsistent pixel buffer: {0}")]
    Buffer(#[from] InvalidRgbBuffer),
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
