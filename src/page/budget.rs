//! Limits that hold before either native library allocates.
//!
//! Both libraries size an allocation from numbers an input controls, and neither fails
//! gracefully when the number is absurd. libjpeg reserves the full source geometry from a
//! header that costs a few bytes to write, and `fast_image_resize::Image::new` allocates
//! with an infallible `vec![0; …]`, so an over-large destination aborts the process rather
//! than returning an error. `catch_unwind` recovers neither.
//!
//! The limits are internal constants rather than options, for the reason `MIN_EDGE` is: a
//! limit a user can raise is a limit that will be raised to force a bad page through.

use super::{Channels, PageErrorKind};

/// The most source pixels a page may declare.
///
/// Chosen, not measured. A `65500x65500` header — 4.29 Gpx from a sub-kilobyte file — has to
/// be refused, and a 600dpi double-page spread, about 70 Mpx, has to be allowed. 100 Mpx
/// sits between them with margin either way.
///
/// Separate from the byte limit because libjpeg's own working buffers follow the *source*
/// geometry, not the scaled output: a progressive image this size holds coefficient arrays
/// for every block whatever `scale_num` asks for.
const MAX_SOURCE_PIXELS: u64 = 100_000_000;

/// The most bytes one image buffer may occupy, decoded or resized.
///
/// Chosen, not measured. A 1280-wide page is about 7 MB decoded; a 600dpi spread about
/// 210 MB. 256 MiB admits that and refuses the 12.87 GB a `65500x65500` RGB header asks
/// for.
///
/// Raising it is not free, and it is not the only term either: this bounds *one buffer*, a
/// worker holds a decoded page and a resize destination at once, and libjpeg's own working
/// set follows the source geometry rather than this — a progressive source at the pixel
/// ceiling holds coefficient arrays of roughly 600 MB inside libjpeg alone, with no backing
/// store, because mozjpeg links `jmemnobs`. Multiply the lot by the worker count.
const MAX_IMAGE_BYTES: u64 = 256 << 20;

/// What one page is allowed to cost.
///
/// Supplied by the caller so it can be exercised directly, but never derived from user
/// input: [`Budget::default`] is the only value the binary uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Budget {
    max_source_pixels: u64,
    max_image_bytes: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_source_pixels: MAX_SOURCE_PIXELS,
            max_image_bytes: MAX_IMAGE_BYTES,
        }
    }
}

impl Budget {
    /// A budget with explicit limits, for tests that would otherwise need a huge fixture.
    #[must_use]
    pub const fn new(max_source_pixels: u64, max_image_bytes: u64) -> Self {
        Self {
            max_source_pixels,
            max_image_bytes,
        }
    }

    /// Rejects a page whose header declares more pixels than the limit allows.
    ///
    /// # Errors
    ///
    /// [`PageErrorKind::TooLarge`], naming the quantity, the value, and the limit.
    pub fn allow_source(&self, width: u32, height: u32) -> Result<(), PageErrorKind> {
        Self::check(
            "source pixels",
            u128::from(width) * u128::from(height),
            self.max_source_pixels,
        )
    }

    /// Rejects a buffer that would exceed the limit, before it is allocated.
    ///
    /// # Errors
    ///
    /// [`PageErrorKind::TooLarge`], naming the quantity, the value, and the limit.
    pub fn allow_image(
        &self,
        width: u32,
        height: u32,
        channels: Channels,
    ) -> Result<(), PageErrorKind> {
        Self::check(
            "image bytes",
            u128::from(width) * u128::from(height) * u128::from(channels.count()),
            self.max_image_bytes,
        )
    }

    /// Rejects a decoded buffer of `bytes`, before the decoder allocates it.
    ///
    /// For a format whose decoder produces samples the encoder cannot take — an alpha
    /// channel, or sixteen bits — the buffer the *decoder* asks for is wider than the page
    /// that comes out of it, so the check has to be on the decoder's own figure rather than
    /// on the page's channel count. Bounding it bounds the page too: narrowing only ever
    /// drops bytes.
    ///
    /// # Errors
    ///
    /// [`PageErrorKind::TooLarge`], naming the quantity, the value, and the limit.
    pub fn allow_decoded(&self, bytes: u64) -> Result<(), PageErrorKind> {
        Self::check("decoded bytes", u128::from(bytes), self.max_image_bytes)
    }

    /// Widened to `u128`, as [`PageImage::new`] is and for the same reason: two `u32` axes
    /// times three channels tops out just under `3 * u64::MAX`, a release build has overflow
    /// checks off, and a wrapped product would compare below the limit and pass. Not
    /// reachable through the binary, whose width is capped and whose JPEG axes are 16-bit,
    /// but this check is what stands between a library caller and an infallible `vec![0; …]`.
    ///
    /// [`PageImage::new`]: super::PageImage::new
    fn check(quantity: &'static str, actual: u128, limit: u64) -> Result<(), PageErrorKind> {
        if actual > u128::from(limit) {
            return Err(PageErrorKind::TooLarge {
                quantity,
                actual,
                limit,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Budget, MAX_IMAGE_BYTES, MAX_SOURCE_PIXELS};
    use crate::page::Channels;

    #[test]
    fn the_defaults_refuse_the_jpeg_maximum_and_allow_a_600dpi_spread() {
        let budget = Budget::default();

        // A structurally valid header may declare this, and libjpeg would reserve
        // 12,870,750,000 bytes for it.
        assert!(budget.allow_source(65500, 65500).is_err());
        assert!(budget.allow_image(65500, 65500, Channels::Rgb).is_err());

        // A 600dpi A4 double-page spread, which is real input.
        assert!(budget.allow_source(9920, 7016).is_ok());
        assert!(budget.allow_image(9920, 7016, Channels::Rgb).is_ok());
    }

    #[test]
    fn the_product_does_not_wrap() {
        // 65536 x 65536 is 2^32, which is 0 in `u32`. Computed in `u32` this would pass.
        let budget = Budget::new(u64::from(u32::MAX), MAX_IMAGE_BYTES);
        let error = budget
            .allow_source(65536, 65536)
            .expect_err("2^32 pixels is over a u32::MAX limit");
        assert!(
            error.to_string().contains("4294967296"),
            "the message must name the real product: {error}"
        );
    }

    #[test]
    fn the_channel_count_is_part_of_the_byte_count() {
        // One geometry, inside a grayscale budget and outside the RGB one.
        let budget = Budget::new(MAX_SOURCE_PIXELS, 1_000_000);
        assert!(budget.allow_image(1000, 1000, Channels::Gray).is_ok());
        assert!(budget.allow_image(1000, 1000, Channels::Rgb).is_err());
    }

    #[test]
    fn the_error_names_the_quantity_and_the_limit() {
        let error = Budget::new(10, MAX_IMAGE_BYTES)
            .allow_source(100, 100)
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("source pixels"), "{message}");
        assert!(message.contains("10000"), "{message}");
        assert!(message.contains("limit of 10"), "{message}");
    }
}
