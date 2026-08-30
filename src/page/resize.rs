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

use super::{PageError, PageErrorKind, RgbImage};

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
    /// Every accepted name, in the order the help text lists them.
    ///
    /// Kept beside [`Filter::from_str`] so the two cannot drift apart; the test below
    /// asserts that each entry parses.
    pub const NAMES: [&'static str; 6] = [
        "nearest-neighbor",
        "bilinear",
        "bicubic",
        "mitchell-netravali",
        "lanczos2",
        "lanczos3",
    ];

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
        match name {
            "nearest-neighbor" => Ok(Self::NearestNeighbor),
            "bilinear" => Ok(Self::Bilinear),
            "bicubic" => Ok(Self::Bicubic),
            "mitchell-netravali" => Ok(Self::MitchellNetravali),
            "lanczos2" => Ok(Self::Lanczos2),
            "lanczos3" => Ok(Self::Lanczos3),
            other => Err(UnknownFilter {
                name: other.to_owned(),
            }),
        }
    }
}

/// A filter name outside the supported six.
///
/// Reported before any archive is opened, so a typo costs nothing.
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

    /// Resizes `source` to `target_width`, deriving the height from the source's aspect
    /// ratio.
    ///
    /// # Errors
    ///
    /// [`PageErrorKind::Resize`] when the crate rejects the buffers or the requested
    /// size — a zero-width or zero-height destination, in practice.
    pub fn resize(
        &mut self,
        name: &str,
        source: &RgbImage,
        target_width: u32,
        filter: Filter,
    ) -> Result<RgbImage, PageError> {
        let target_height = height_for_width(source.width(), source.height(), target_width);

        let view = ImageRef::new(
            source.width(),
            source.height(),
            source.pixels(),
            PixelType::U8x3,
        )
        .map_err(|error| PageError::new(name, PageErrorKind::Resize(error.to_string())))?;

        let mut destination = Image::new(target_width, target_height, PixelType::U8x3);
        let options = ResizeOptions::new().resize_alg(filter.resize_alg());
        self.resizer
            .resize(&view, &mut destination, &options)
            .map_err(|error| PageError::new(name, PageErrorKind::Resize(error.to_string())))?;

        RgbImage::new(target_width, target_height, destination.into_vec())
            .map_err(|error| PageError::new(name, error.into()))
    }
}

/// The height that keeps `src_width` × `src_height`'s aspect ratio at `target_width`.
///
/// Rounded to nearest, halves away from zero. Computed in integer arithmetic rather than
/// through `f64::round` so a one-pixel difference cannot depend on the platform's
/// floating-point rounding mode.
///
/// Returns 0 for a zero-width source, which no JPEG has.
#[must_use]
pub fn height_for_width(src_width: u32, src_height: u32, target_width: u32) -> u32 {
    if src_width == 0 {
        return 0;
    }

    let numerator = u64::from(src_height) * u64::from(target_width);
    let denominator = u64::from(src_width);
    // `+ denominator / 2` is exact half-up for even denominators, and correct for odd
    // ones because a ratio with an odd denominator can never land on a half.
    let rounded = (numerator + denominator / 2) / denominator;

    u32::try_from(rounded).unwrap_or(u32::MAX)
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

    use super::{Filter, height_for_width, lanczos2, sinc};

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
    fn every_advertised_name_parses() {
        for name in Filter::NAMES {
            assert!(
                name.parse::<Filter>().is_ok(),
                "{name} is advertised but does not parse"
            );
        }
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
    fn a_zero_width_source_has_no_height() {
        assert_eq!(height_for_width(0, 2150, 1280), 0);
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
