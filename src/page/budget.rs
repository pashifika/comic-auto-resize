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
//!
//! # Two limits bound one page, and which binds first depends on the stored form
//!
//! These are not the only limit a page meets. [`MAX_ENTRY_BYTES`] bounds the *entry* a page
//! arrives in, and for a format whose entries carry raw pixels it is reached at a far smaller
//! pixel count than the limits here state. Measured, a 24-bit bmp entry reaches 64 MiB at
//! **22,369,603 pixels** — 4096x5461 fits at 67,104,822 B and 4096x5462 does not at
//! 67,117,110 B, refused by name — so a 24-bit bmp's reachable ceiling is 22% of the 100 Mpx
//! `MAX_SOURCE_PIXELS` advertises. png, webp and JPEG carry the same page in kilobytes and
//! reach their own ceiling first.
//!
//! **That ordering is per stored form and not per format.** `image` accepts 1-, 2-, 4-, 8-,
//! 16-, 24- and 32-bit bmp along with RLE4 and RLE8, and expands every palettised form to
//! `Rgb8`, so a shallower page carries far more pixels in the same entry:
//!
//! | stored form | entry ceiling at 64 MiB | what actually binds first |
//! |---|---|---|
//! | 32-bit | 16.8 Mpx | the entry limit |
//! | 24-bit | 22,369,603 px | the entry limit |
//! | 16-bit | 33.6 Mpx | the entry limit |
//! | 8-bit palettised | 67.1 Mpx | the entry limit |
//! | 4-bit, 2-bit, 1-bit | 134 Mpx and up | the **decoded-byte** limit, at 89,478,485 px |
//! | RLE4, RLE8 | content-dependent | either one, decided by the run structure |
//!
//! **The RLE row is content-dependent in both directions, which is why it names both limits.**
//! A run is a count byte and a palette index (`image-0.25.10/src/codecs/bmp/decoder.rs:1197-1200`)
//! and a count of one is legal, so a page stored entirely as one-pixel runs spends two entry
//! bytes a pixel and meets the entry limit at about 33.5 Mpx — half the ceiling of the
//! uncompressed 8-bit form RLE8 compresses. The same page stored as long runs spends two bytes
//! for as many as 255 pixels and meets the decoded-byte limit at 89,478,485 px, like the row
//! above it. The two swap at an average run of about three pixels, where `2/L` entry bytes a
//! pixel for a run length `L` crosses `67,108,864 / 89,478,485`; which side a page lands on is
//! the encoder's choice, so neither limit can be named as the one that binds.
//!
//! Measured on the release binary with a 1-bit palettised fixture: 10000x10001 is refused with
//! `source pixels is 100010000, over the limit of 100000000`, 10000x10000 is refused with
//! `decoded bytes is 300000000, over the limit of 268435456`, and 9000x9000 — whose entry is
//! 10,152,062 B — is accepted and decodes to 243 MB. So a bmp *can* be refused by the pixel
//! limit. What holds unqualified is only that an **uncompressed** page at 24 bits or deeper
//! meets the entry limit first, and `MAX_SOURCE_PIXELS` twenty lines below says so in those
//! terms.
//!
//! Every one of those refusals is correct and every one stays. Stated here because a limit
//! whose effective value depends on the stored form is a limit a reader will otherwise get
//! wrong, and because the refusals name different things: the entry limit names the entry and
//! its own byte limit, and these name the quantity, the value and the limit.
//!
//! [`MAX_ENTRY_BYTES`]: crate::source::MAX_ENTRY_BYTES

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
///
/// **Not reachable for every stored form.** See the module documentation: an uncompressed page
/// at 24 bits a stored pixel or deeper meets
/// [`MAX_ENTRY_BYTES`](crate::source::MAX_ENTRY_BYTES) at 22.4 Mpx and never gets here. A
/// shallower stored form — a 1-bit or 4-bit palette, or an RLE run structure that unpacks far
/// more than it stores — does reach this limit, and `budget.rs`'s own tests exercise it.
const MAX_SOURCE_PIXELS: u64 = 100_000_000;

/// The most bytes one image buffer may occupy, decoded or resized.
///
/// Chosen, not measured. A 1280-wide page is about 7 MB decoded; a 600dpi spread about
/// 210 MB. 256 MiB admits that and refuses the 12.87 GB a `65500x65500` RGB header asks
/// for.
///
/// Raising it is not free, and it is not the only term either: this bounds *one buffer*, a
/// worker holds a decoded page and a resize destination at once, and libjpeg's own working
/// set follows the *source* geometry rather than this — a **progressive** source holds
/// coefficient arrays for every block whatever `scale_num` asks for, and this crate's encoder
/// writes progressive, so the tool's own output fed back in is the expensive case. Measured on
/// one 6000x8000 page through the pipeline, the same picture costs 43.7 MB as a baseline JPEG
/// and 186.4 MB as a progressive one: 2.97 bytes a pixel, which is 4:2:0's 1.5 samples at two
/// bytes a coefficient, so about 300 MB at the pixel ceiling above. There is no backing store,
/// because mozjpeg links `jmemnobs`.
///
/// That term is **recorded and not charged**, and the distinction is deliberate: the arm is
/// selected by a progressive flag `mozjpeg`'s `Decompress` does not expose, so charging it
/// needs the dependency to offer it first. The three decoders whose excess *is* readable before
/// the decode are charged — see [`Self::allow_decoded`]. Multiply the lot by the worker count:
/// the run's peak is a product, and `pipeline`'s module documentation states it with every
/// factor named and measured.
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

    /// Rejects a decode whose peak is `bytes`, before any of it is allocated.
    ///
    /// **The peak, not one buffer.** For a format whose decoder produces samples the encoder
    /// cannot take — an alpha channel, or sixteen bits — the narrowing builds the page's buffer
    /// while the decoder's is still alive, so both are charged; where the decoder's buffer is
    /// already what the encoder takes it is *moved* rather than copied and only one is. An
    /// earlier version of this checked the decoder's buffer alone and reasoned that narrowing
    /// only drops bytes, which is true of the *result* and not of the moment both exist — an
    /// independent review caught it, and the correction is here rather than in a comment
    /// because the spec's clause is about every allocation an input sizes.
    ///
    /// **And the decoder's own allocations, not only the buffer it declares.** A decoder may
    /// allocate around the buffer `image` hands it — scratch it copies down from, or transform
    /// buffers on the way — and that excess is sized by the input like everything else. So
    /// `bytes` is the declared buffer times a factor **measured for the arm the input
    /// selected**, plus the page where the two coexist. The factors and the method are in
    /// `page::decode`'s raster module; one arm is above one, and charging its factor to every
    /// arm would refuse pages that do not incur it.
    ///
    /// # Errors
    ///
    /// [`PageErrorKind::TooLarge`], naming the quantity, the value, and the limit.
    pub fn allow_decoded(&self, bytes: u128) -> Result<(), PageErrorKind> {
        Self::check("decoded bytes", bytes, self.max_image_bytes)
    }

    /// The most bytes one image buffer may occupy.
    ///
    /// Exposed for one caller: `image`'s png decoder inflates an `iCCP` chunk during *header*
    /// parsing, before its dimensions are readable, and the only way to bound that is to hand
    /// the decoder a pool of its own. This is the **cap** on that pool rather than its size —
    /// `page::decode`'s raster module sizes it from the entry's own length and clamps it here,
    /// so the clamp never binds in the binary and a test can lower it to exercise the pool.
    #[must_use]
    pub const fn max_image_bytes(&self) -> u64 {
        self.max_image_bytes
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
    use crate::source::{MAX_ENTRY_BYTES, SourceError};

    /// What a 24-bit bmp entry of `width` x `height` occupies: two headers, bottom-up BGR,
    /// rows padded to four bytes. The arithmetic the ceiling below comes out of.
    fn bmp_entry_bytes(width: u64, height: u64) -> u64 {
        54 + height * ((width * 3 + 3) & !3)
    }

    /// What a 1-bit palettised bmp entry of `width` x `height` occupies: the same headers, a
    /// two-entry palette, and rows of packed bits padded to four bytes.
    ///
    /// The shallow end of the range `image` accepts, and the reason the ordering in the module
    /// documentation is stated per stored form rather than per format.
    fn bmp_1bit_entry_bytes(width: u64, height: u64) -> u64 {
        62 + height * (4 * width.div_ceil(32))
    }

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

    /// For a 24-bit uncompressed page the *entry* limit is reached first, and by a wide margin
    /// — and for a shallower one it is not reached at all.
    ///
    /// The two limits are independent — one bounds the bytes an entry occupies, the other the
    /// pixels a page may declare — and nothing makes them agree. Measured on the release binary
    /// at the boundary row: 4096x5461 is accepted and 4096x5462 is refused with `entry is
    /// larger than the limit of 67108864 bytes`, both far inside every pixel limit here.
    ///
    /// The second half is the assertion whose absence let a false absolute ship. `image`
    /// expands a palettised bmp to `Rgb8`, so at one bit a stored pixel the entry limit stops
    /// binding and the byte limit takes over. This pins a page the 24-bit form could never have
    /// carried as inside the entry limit and outside the byte limit, which is precisely the
    /// case the words "a bmp can never be refused by the pixel limit" denied.
    ///
    /// Asserted as arithmetic rather than through a 64 MiB fixture: the boundary needs an entry
    /// at the limit, which is two orders of magnitude past every other fixture in this suite
    /// and does not belong in a suite CI runs on three hosts. The end-to-end runs at every row
    /// named here are recorded with the Change's measurements.
    #[test]
    fn the_entry_limit_binds_before_the_pixel_limit_for_an_uncompressed_page() {
        // The last row that fits, and the first that does not.
        assert_eq!(bmp_entry_bytes(4096, 5461), 67_104_822);
        assert_eq!(bmp_entry_bytes(4096, 5462), 67_117_110);
        assert!(bmp_entry_bytes(4096, 5461) <= MAX_ENTRY_BYTES);
        assert!(bmp_entry_bytes(4096, 5462) > MAX_ENTRY_BYTES);

        // And the page the entry limit refused is inside both of this module's limits, so the
        // entry limit is the only thing that could have refused it.
        let budget = Budget::default();
        assert!(budget.allow_source(4096, 5462).is_ok());
        assert!(budget.allow_image(4096, 5462, Channels::Rgb).is_ok());

        // Which makes the pixel ceiling unreachable at this depth, not merely lower: an entry
        // holding 100 Mpx of uncompressed 24-bit BGR is four and a half times the entry limit.
        assert!(
            bmp_entry_bytes(10_000, 10_000) > MAX_ENTRY_BYTES * 4,
            "a 100 Mpx 24-bit bmp entry must be far past the entry limit for the claim to hold"
        );
        const {
            assert!(
                (MAX_ENTRY_BYTES - 54) / 3 < MAX_SOURCE_PIXELS / 4,
                "a 24-bit bmp's effective ceiling must be a small fraction of the stated one"
            );
        }

        // At one bit a stored pixel the same 100 Mpx page occupies 12.5 MB, so it clears the
        // entry limit and the pixel limit and is refused by what its expansion to `Rgb8` costs.
        // Measured on the release binary with a 1-bit fixture: `decoded bytes is 300000000,
        // over the limit of 268435456` here, and `source pixels is 100010000` one row taller.
        assert_eq!(bmp_1bit_entry_bytes(10_000, 10_000), 12_520_062);
        assert!(bmp_1bit_entry_bytes(10_000, 10_000) < MAX_ENTRY_BYTES);
        assert!(budget.allow_source(10_000, 10_000).is_ok());
        assert!(
            budget
                .allow_decoded(u128::from(10_000u64 * 10_000 * 3))
                .is_err(),
            "a bmp does reach these limits; only a 24-bit one cannot"
        );

        // The accepted side of that boundary, so the refusal above is a boundary rather than a
        // blanket: 9000x9000 is a 10,152,062 B entry that decodes to 243 MB and is allowed.
        assert_eq!(bmp_1bit_entry_bytes(9_000, 9_000), 10_152_062);
        assert!(budget.allow_source(9_000, 9_000).is_ok());
        assert!(
            budget
                .allow_decoded(u128::from(9_000u64 * 9_000 * 3))
                .is_ok()
        );
    }

    /// Two limits, two refusals, and neither borrows the other's words.
    ///
    /// A page inside the pixel limit that its entry's size refused has to say so: the reader's
    /// message names the entry and its byte limit, and this module's names the quantity, the
    /// value and the limit. A reader who sees one must not have to guess which limit fired.
    #[test]
    fn the_two_limits_are_not_conflated_in_a_refusal() {
        let entry = SourceError::TooLarge {
            name: "pages/page0000.bmp".to_owned(),
            limit: MAX_ENTRY_BYTES,
        }
        .to_string();
        let pixels = Budget::new(10, MAX_IMAGE_BYTES)
            .allow_source(4096, 5462)
            .expect_err("22 Mpx is over a 10 pixel limit")
            .to_string();

        assert!(entry.contains("entry is larger than the limit"), "{entry}");
        assert!(entry.contains("67108864"), "{entry}");
        assert!(pixels.contains("source pixels"), "{pixels}");

        for quantity in ["source pixels", "image bytes", "decoded bytes"] {
            assert!(
                !entry.contains(quantity),
                "the entry refusal claims a pixel quantity: {entry}"
            );
        }
        assert!(
            !pixels.contains("entry"),
            "a pixel refusal must not read as the entry limit: {pixels}"
        );
    }
}
