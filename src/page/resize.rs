//! Resampling, through `fast_image_resize`.
//!
//! Reducing pixel count is where the size reduction comes from, so the filter set is the
//! one the Go implementation shipped rather than whatever the crate happens to provide.
//! Two of the six names do not map onto a `FilterType` variant: nearest neighbour is an
//! algorithm rather than a convolution, and Lanczos2 does not exist in the crate and is
//! built from its kernel.

use std::f64::consts::PI;
use std::str::FromStr;

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{
    Filter as Kernel, FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
};
use thiserror::Error;

use super::{Channels, PageError, PageErrorKind, PageImage};

/// The resampling filters the tool offers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Filter {
    NearestNeighbor,
    Bilinear,
    Bicubic,
    MitchellNetravali,
    Lanczos2,
    /// The default, as in the Go implementation.
    #[default]
    Lanczos3,
}

impl Filter {
    /// Every variant, in the order the help text lists them.
    ///
    /// [`Filter::NAMES`], the [`UnknownFilter`] message, and [`Filter::from_str`] are all
    /// derived from this array through [`Filter::name`], so a name cannot exist without a
    /// parse route or a parse route without a name, and a seventh variant cannot compile
    /// at all until it is named — `name` and [`Filter::resize_alg`] are both exhaustive
    /// matches with no catch-all. Listing the variant here is the one remaining manual
    /// step, and the test below walks this array, so a wrong or duplicated name fails
    /// there rather than shipping.
    pub const ALL: [Self; 6] = [
        Self::NearestNeighbor,
        Self::Bilinear,
        Self::Bicubic,
        Self::MitchellNetravali,
        Self::Lanczos2,
        Self::Lanczos3,
    ];

    /// Every accepted name, in the order the help text lists them.
    ///
    /// Built from [`Filter::ALL`] rather than written out, so it cannot disagree with
    /// [`Filter::name`].
    pub const NAMES: [&'static str; Self::ALL.len()] = {
        let mut names = [""; Self::ALL.len()];
        let mut index = 0;
        while index < Self::ALL.len() {
            names[index] = Self::ALL[index].name();
            index += 1;
        }
        names
    };

    /// The name the tool's filter flag accepts for this filter.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NearestNeighbor => "nearest-neighbor",
            Self::Bilinear => "bilinear",
            Self::Bicubic => "bicubic",
            Self::MitchellNetravali => "mitchell-netravali",
            Self::Lanczos2 => "lanczos2",
            Self::Lanczos3 => "lanczos3",
        }
    }

    /// Maps a filter onto the crate's algorithm.
    ///
    /// `CatmullRom` is the bicubic the Go implementation means. Substituting `Lanczos3`
    /// for `lanczos2`, or `Bilinear` for either, would make the flag a lie.
    fn resize_alg(self) -> ResizeAlg {
        match self {
            // Nearest is not a convolution, so it has no `FilterType`.
            Self::NearestNeighbor => ResizeAlg::Nearest,
            Self::Bilinear => ResizeAlg::Convolution(FilterType::Bilinear),
            Self::Bicubic => ResizeAlg::Convolution(FilterType::CatmullRom),
            Self::MitchellNetravali => ResizeAlg::Convolution(FilterType::Mitchell),
            Self::Lanczos2 => ResizeAlg::Convolution(FilterType::Custom(lanczos2_kernel())),
            Self::Lanczos3 => ResizeAlg::Convolution(FilterType::Lanczos3),
        }
    }
}

impl FromStr for Filter {
    type Err = UnknownFilter;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|filter| filter.name() == name)
            .ok_or_else(|| UnknownFilter {
                name: name.to_owned(),
            })
    }
}

/// A filter name outside the supported six.
///
/// Carries the rejected name and lists the accepted ones, so the message is actionable
/// without a second lookup. *When* it is reported relative to reading an image is the
/// caller's ordering, not this type's.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("unknown resize filter `{name}`; supported: {}", Filter::NAMES.join(", "))]
pub struct UnknownFilter {
    pub name: String,
}

/// A resampler that keeps its scratch buffers between pages.
///
/// `fast_image_resize`'s `Resizer` owns the buffers its convolutions work in, so the
/// pipeline holds one per worker thread rather than building one per page.
#[derive(Debug, Default)]
pub struct Resampler {
    resizer: Resizer,
}

impl Resampler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resizes `source` to `target_width`, keeping its channel count and the aspect ratio
    /// of the page it came from.
    ///
    /// The height is derived here, not requested. It comes from
    /// [`PageImage::original_width`] and [`PageImage::original_height`] — the geometry the
    /// page declared in its *header* — rather than from `source`'s own dimensions, which
    /// are a different thing whenever `source` came out of a scaled decode: libjpeg rounds
    /// each axis up independently, so a 1463x1800 page decoded at `7/8` arrives as
    /// 1281x1575, and deriving the height from *that* at width 1280 gives 1574 where the
    /// original ratio demands 1575.
    ///
    /// There is deliberately no second target axis. `target_width` is the only size a
    /// caller names, so the destination this allocates stays a function of `source`'s
    /// recorded geometry and that one number. A [`PageImage`] built directly through
    /// [`PageImage::new`] records its own actual geometry as the original, and the only
    /// method that records anything else is crate-private — so downstream code cannot
    /// *forge* a mismatch, though [`decode`](crate::page::decode) does legitimately return
    /// one when it applied a scaled decode. Before the recorded geometry is used it is
    /// checked to be geometry `source` could be a scaled decode of, which bounds each
    /// recorded *axis*, and the unrounded ratio, at eight times `source`'s. That is a
    /// per-axis bound rather than exact provenance: it does not bound the derived integer
    /// height at eight times the buffer-derived one, because each axis rounds half-up
    /// independently. A `200x1` buffer recorded as `200x8` passes the check, and at width
    /// 299 derives 12 where the buffer alone would derive 1.
    ///
    /// A three-byte page therefore resizes to exactly 1280x1280 at width 1280 and cannot
    /// be made to reach further, because the sentence that would say otherwise does not
    /// compile outside this crate:
    ///
    /// ```
    /// use comic_auto_resize::page::{Channels, Filter, PageImage, Resampler};
    ///
    /// let tiny = PageImage::new(1, 1, Channels::Rgb, vec![0; 3])?;
    /// let resized = Resampler::new().resize("tiny.jpg", &tiny, 1280, Filter::default())?;
    /// assert_eq!((resized.width(), resized.height()), (1280, 1280));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// ```compile_fail,E0624
    /// use comic_auto_resize::page::{Channels, PageImage};
    ///
    /// let tiny = PageImage::new(1, 1, Channels::Rgb, vec![0; 3]).unwrap();
    /// // `scaled_from` is crate-private, so no consumer can claim a page larger than the
    /// // pixels it is holding, and there is no target height to name instead.
    /// let lying = tiny.scaled_from(1, u32::MAX);
    /// ```
    ///
    /// # Errors
    ///
    /// [`PageErrorKind::Resize`] when `source`'s recorded page geometry is not one it
    /// could be a scaled decode of; when `target_width` is zero, or `source` has an axis
    /// with no extent, either of which leaves the derived target size zero on an axis —
    /// `fast_image_resize` treats a zero-sized destination as a successful no-op, and a
    /// page with no pixels is not a resized page; or when the crate rejects the buffers for
    /// any other reason.
    pub fn resize(
        &mut self,
        name: &str,
        source: &PageImage,
        target_width: u32,
        filter: Filter,
    ) -> Result<PageImage, PageError> {
        let (original_width, original_height) = original_size(name, source)?;
        let target_height = height_for_width(original_width, original_height, target_width);
        if target_width == 0 || target_height == 0 {
            return Err(PageError::new(
                name,
                PageErrorKind::Resize(format!(
                    "target size {target_width}x{target_height}, derived from a \
                     {original_width}x{original_height} page, has a zero axis"
                )),
            ));
        }

        let pixel_type = match source.channels() {
            Channels::Gray => PixelType::U8,
            Channels::Rgb => PixelType::U8x3,
        };

        let view = ImageRef::new(source.width(), source.height(), source.pixels(), pixel_type)
            .map_err(|error| PageError::new(name, PageErrorKind::Resize(error.to_string())))?;

        let mut destination = Image::new(target_width, target_height, pixel_type);
        let options = ResizeOptions::new().resize_alg(filter.resize_alg());
        self.resizer
            .resize(&view, &mut destination, &options)
            .map_err(|error| PageError::new(name, PageErrorKind::Resize(error.to_string())))?;

        PageImage::new(
            target_width,
            target_height,
            source.channels(),
            destination.into_vec(),
        )
        .map_err(|error| PageError::new(name, error.into()))
    }
}

/// `source`'s recorded page geometry, once it has been checked against `source` itself.
///
/// A scaled decode is the only thing that makes the two differ, and it can only shrink,
/// by a numerator over eight: each output axis is `ceil(original * numerator / 8)` for one
/// numerator in `1..=8`. So every axis of a page `source` could be a decode of lies in
/// `axis..=axis * 8` — no smaller, because a decode does not enlarge, and no more than
/// eight times larger, because there is no `1/9`.
///
/// Checking it is what keeps the derived height tied to `source`. A recorded original of
/// `1 x u32::MAX` would otherwise send a three-byte buffer at width 1280 into
/// [`Image::new`], which allocates `1280 * 4_294_967_295 * 3` bytes with an infallible
/// `vec![0; …]` on a path that has no unwind boundary to recover through.
fn original_size(name: &str, source: &PageImage) -> Result<(u32, u32), PageError> {
    let (width, height) = (source.original_width(), source.original_height());
    let decodable = |buffer: u32, original: u32| {
        original >= buffer && u64::from(original) <= u64::from(buffer) * 8
    };

    if decodable(source.width(), width) && decodable(source.height(), height) {
        Ok((width, height))
    } else {
        Err(PageError::new(
            name,
            PageErrorKind::Resize(format!(
                "a {}x{} buffer is not a scaled decode of a {width}x{height} page",
                source.width(),
                source.height()
            )),
        ))
    }
}

/// The height that keeps `src_width` × `src_height`'s aspect ratio at `target_width`.
///
/// Rounded to nearest, halves away from zero. Computed in integer arithmetic rather than
/// through `f64::round` so a one-pixel difference cannot depend on the platform's
/// floating-point rounding mode.
///
/// The result is then clamped up to 1: a 30000x1 page at width 1280 rounds to 0, and a
/// destination with no rows is not a thinner page but a lost one. It returns 0 only when
/// there is no ratio to preserve — a zero source axis, which no JPEG has, or a zero
/// `target_width`, which [`Resampler::resize`] rejects.
///
/// It saturates rather than preserving the ratio when the exact height exceeds `u32::MAX`:
/// `height_for_width(1, u32::MAX, u32::MAX)` is `u32::MAX`, not `u32::MAX` squared. A JPEG
/// cannot reach that — its axes are 16-bit — so the saturation is only reachable from a
/// `target_width` no page has, and it is pinned by a test rather than left to be found.
#[must_use]
pub fn height_for_width(src_width: u32, src_height: u32, target_width: u32) -> u32 {
    if src_width == 0 || src_height == 0 || target_width == 0 {
        return 0;
    }

    let numerator = u64::from(src_height) * u64::from(target_width);
    let denominator = u64::from(src_width);
    // `+ denominator / 2` is exact half-up for even denominators, and correct for odd
    // ones because a ratio with an odd denominator can never land on a half.
    let rounded = (numerator + denominator / 2) / denominator;

    u32::try_from(rounded.max(1)).unwrap_or(u32::MAX)
}

/// Lanczos2: a sinc windowed by a sinc of twice the period, truncated at radius 2.
///
/// The crate's `FilterType` stops at Lanczos3, and its own documentation gives the
/// Lanczos4 construction, so this is the supported way to add one rather than a
/// workaround.
fn lanczos2_kernel() -> Kernel {
    Kernel::new("Lanczos2", lanczos2, 2.0).expect("a support radius of 2.0 is finite and positive")
}

fn lanczos2(x: f64) -> f64 {
    if (-2.0..2.0).contains(&x) {
        sinc(x) * sinc(x / 2.0)
    } else {
        0.0
    }
}

// The comparison against zero is exact on purpose: `sinc` has a removable singularity at
// exactly 0, and widening the guard to a tolerance would return 1.0 for nearby values
// whose real sinc is not 1.0.
// (`clippy::float_cmp` does not fire on a literal-zero guard, so no allow is needed.)
fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    let x = x * PI;
    x.sin() / x
}

#[cfg(test)]
mod tests {
    use fast_image_resize::{FilterType, ResizeAlg};

    use super::{
        Channels, Filter, PageErrorKind, PageImage, Resampler, height_for_width, lanczos2, sinc,
    };

    /// A 160x240 grayscale buffer, the shape every coherence case below starts from.
    fn buffer() -> PageImage {
        PageImage::new(160, 240, Channels::Gray, vec![0; 160 * 240]).expect("160 * 240 * 1")
    }

    /// The F5 trigger, attempted in the only place it can still be reached from.
    ///
    /// [`PageImage::scaled_from`] is crate-private, so no consumer of this crate can hand
    /// `resize` a page geometry its buffer could not be a decode of. This reaches for it
    /// from inside, and each recorded original outside `axis..=axis * 8` is rejected
    /// instead of becoming a destination `fast_image_resize` allocates.
    #[test]
    fn a_recorded_original_the_buffer_cannot_be_a_decode_of_is_rejected() {
        let mut resampler = Resampler::new();

        for (original_width, original_height) in [
            // The F5 allocation, laundered through the ratio instead of named outright.
            (160, u32::MAX),
            // One row past `1/8`, so the bound is exact rather than approximate.
            (160, 1921),
            (161 * 8, 240),
            // Smaller than the buffer on an axis, which no scaled decode can produce.
            (80, 240),
            (160, 239),
        ] {
            let error = resampler
                .resize(
                    "odd.jpg",
                    &buffer().scaled_from(original_width, original_height),
                    1280,
                    Filter::default(),
                )
                .expect_err("an incoherent page geometry is not a resize request");
            assert!(matches!(error.kind, PageErrorKind::Resize(_)));
            assert!(
                error.to_string().contains("odd.jpg"),
                "the message must name the input: {error}"
            );
        }
    }

    /// The other side of that boundary, so the check is tight rather than merely present:
    /// exactly `1/8` on both axes is a decode libjpeg performs, and is honoured.
    #[test]
    fn a_recorded_original_exactly_eight_times_the_buffer_is_a_decode_that_happens() {
        let eighth = buffer().scaled_from(160 * 8, 240 * 8);
        let resized = Resampler::new()
            .resize("eighth.jpg", &eighth, 1280, Filter::default())
            .expect("an eighth-size decode is a real decode");
        assert_eq!((resized.width(), resized.height()), (1280, 1920));
    }

    /// A resize output records itself, so feeding one back in is bounded by its own
    /// geometry rather than compounding the original it was derived from.
    #[test]
    fn a_resized_page_is_its_own_original() {
        let resized = Resampler::new()
            .resize(
                "eighth.jpg",
                &buffer().scaled_from(160 * 8, 240 * 8),
                160,
                Filter::default(),
            )
            .expect("resizes");
        assert_eq!(
            (resized.original_width(), resized.original_height()),
            (resized.width(), resized.height())
        );
    }

    #[test]
    fn each_filter_maps_to_the_algorithm_the_design_names() {
        assert_eq!(Filter::NearestNeighbor.resize_alg(), ResizeAlg::Nearest);
        assert_eq!(
            Filter::Bilinear.resize_alg(),
            ResizeAlg::Convolution(FilterType::Bilinear)
        );
        // The bicubic the Go implementation means, not `Mitchell`.
        assert_eq!(
            Filter::Bicubic.resize_alg(),
            ResizeAlg::Convolution(FilterType::CatmullRom)
        );
        assert_eq!(
            Filter::MitchellNetravali.resize_alg(),
            ResizeAlg::Convolution(FilterType::Mitchell)
        );
        assert_eq!(
            Filter::Lanczos3.resize_alg(),
            ResizeAlg::Convolution(FilterType::Lanczos3)
        );
    }

    #[test]
    fn lanczos2_reaches_the_resizer_as_a_custom_kernel_of_support_two() {
        let ResizeAlg::Convolution(FilterType::Custom(kernel)) = Filter::Lanczos2.resize_alg()
        else {
            panic!("lanczos2 must reach the resizer as a custom convolution kernel");
        };

        // The spec forbids approximating it with `Lanczos3` or `Bilinear`, both of which
        // would arrive here as a named variant rather than as `Custom`.
        assert_eq!(kernel.name(), "Lanczos2");
        // The radius is stored verbatim by `Filter::new`, so a one-ULP window is the
        // strictest form this can take without tripping `clippy::float_cmp`.
        assert!(
            (kernel.support() - 2.0).abs() < f64::EPSILON,
            "support was {}, not 2.0",
            kernel.support()
        );
    }

    #[test]
    fn every_variant_is_named_once_and_parses_back_to_itself() {
        assert_eq!(Filter::NAMES.len(), Filter::ALL.len());

        for (filter, name) in Filter::ALL.into_iter().zip(Filter::NAMES) {
            assert_eq!(filter.name(), name, "NAMES disagrees with name()");
            assert_eq!(
                name.parse::<Filter>().expect("an advertised name parses"),
                filter,
                "{name} parses to the wrong variant"
            );
        }

        // Two variants sharing a name would make one of them unreachable through the flag
        // while every other assertion above still held.
        let mut names = Filter::NAMES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            Filter::NAMES.len(),
            "two variants share a name: {:?}",
            Filter::NAMES
        );
    }

    #[test]
    fn an_unknown_name_lists_the_supported_ones() {
        let error = "lanczos4"
            .parse::<Filter>()
            .expect_err("should be rejected");
        let message = error.to_string();
        for name in Filter::NAMES {
            assert!(message.contains(name), "{message} does not mention {name}");
        }
    }

    #[test]
    fn the_default_is_lanczos3() {
        assert_eq!(Filter::default(), Filter::Lanczos3);
    }

    #[test]
    fn aspect_ratio_survives_the_resize() {
        assert_eq!(height_for_width(1520, 2150, 1280), 1811);
    }

    #[test]
    fn rounding_is_to_nearest_and_away_from_zero() {
        // 3 * 1 / 2 is exactly 1.5, which must become 2 rather than 1.
        assert_eq!(height_for_width(2, 3, 1), 2);
        // 5 * 1 / 4 is 1.25, which must become 1.
        assert_eq!(height_for_width(4, 5, 1), 1);
        // 7 * 1 / 4 is 1.75, which must become 2.
        assert_eq!(height_for_width(4, 7, 1), 2);
    }

    #[test]
    fn a_source_with_no_area_has_no_height() {
        assert_eq!(height_for_width(0, 2150, 1280), 0);
        assert_eq!(height_for_width(1520, 0, 1280), 0);
        assert_eq!(height_for_width(1520, 2150, 0), 0);
    }

    #[test]
    fn an_extreme_aspect_ratio_keeps_one_row() {
        // 1 * 1280 / 30000 rounds to 0. A page that thin still has a row.
        assert_eq!(height_for_width(30000, 1, 1280), 1);
        // The nearest ratio that does *not* need the clamp, so the clamp is not just
        // returning 1 for everything small.
        assert_eq!(height_for_width(2560, 1, 1280), 1);
        assert_eq!(height_for_width(2560, 3, 1280), 2);
    }

    /// Pins a limitation rather than a feature: see the saturation note on
    /// [`height_for_width`].
    #[test]
    fn a_height_beyond_u32_saturates_rather_than_wrapping() {
        assert_eq!(height_for_width(1, u32::MAX, u32::MAX), u32::MAX);
    }

    #[test]
    fn the_lanczos2_kernel_is_a_lanczos_kernel_of_radius_two() {
        assert!((sinc(0.0) - 1.0).abs() < 1e-12);
        assert!((lanczos2(0.0) - 1.0).abs() < 1e-12);
        // A sinc-windowed sinc has zeros at every non-zero integer inside its support.
        assert!(lanczos2(1.0).abs() < 1e-12);
        // Nothing outside the radius contributes, which is what makes the support 2.
        assert!(lanczos2(2.0) == 0.0, "radius 2 must not contribute");
        assert!(lanczos2(-2.5) == 0.0, "beyond radius 2 must not contribute");
        // Between the zeros it is negative, as a windowed sinc must be.
        assert!(lanczos2(1.5) < 0.0);
    }
}
