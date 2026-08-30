//! What size a page becomes, and when it is left alone.
//!
//! Normalisation is on width only. There is no height basis and no automatic basis
//! selection: a comic page's width is what a reader's viewport constrains, and the height
//! follows from the source's aspect ratio.

use crate::page::height_for_width;

/// The default target width, as in the Go implementation.
pub const AUTO_WIDTH: u32 = 1280;

/// The smallest edge a *resized* page may end up with.
///
/// Replaces Go's `reW <= 500 || reH <= 100`, an asymmetric pair that acted as an aspect
/// filter in auto mode and an absolute floor in ratio mode — two behaviours from one test.
/// One symmetric floor does the job the pair was reaching for.
///
/// Not a command-line option, for the same reason the budget limits are not: a limit a user
/// can raise is a limit that will be raised to force a bad page through. A page below it
/// passes through at full size, which is the conservative outcome, so there is nothing to
/// tune.
const MIN_EDGE: u32 = 250;

/// What happens to one page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Plan {
    /// Resize to this width, then encode. The height is not carried: [`Resampler::resize`]
    /// derives it from the page's own recorded geometry, and two copies of one number are
    /// two chances to disagree.
    ///
    /// [`Resampler::resize`]: crate::page::Resampler::resize
    Resize { width: u32 },
    /// Encode at the source dimensions, without resizing.
    PassThrough,
}

impl Plan {
    /// The size a scaled decode may aim for, or `None` when the page is not being resized.
    ///
    /// Both axes, because libjpeg's step selection has to clear the target on each.
    #[must_use]
    pub fn scale_to(self, src_width: u32, src_height: u32) -> Option<(u32, u32)> {
        match self {
            Self::Resize { width } => Some((width, height_for_width(src_width, src_height, width))),
            Self::PassThrough => None,
        }
    }
}

/// Decides what to do with a `src_width` × `src_height` page aiming at `target_width`.
///
/// Pass-through when the page is already at or below the target, or when the smaller edge of
/// the result would fall below [`MIN_EDGE`]. Pass-through skips the resize and nothing else:
/// the encoder always runs, which is what makes "recompress without resizing" a request the
/// tool honours rather than one that silently becomes a copy.
#[must_use]
pub fn plan(src_width: u32, src_height: u32, target_width: u32) -> Plan {
    // A zero axis has no ratio to preserve, and no JPEG has one. Passing through leaves the
    // page for the encoder to reject rather than inventing a size for it here.
    if src_width == 0 || src_height == 0 || target_width == 0 {
        return Plan::PassThrough;
    }

    // The scale is `target_width / src_width`; at or above 1.0 there is nothing to reduce.
    // Compared as integers, so the decision cannot turn on a floating-point rounding mode.
    if target_width >= src_width {
        return Plan::PassThrough;
    }

    let target_height = height_for_width(src_width, src_height, target_width);
    if target_width.min(target_height) < MIN_EDGE {
        return Plan::PassThrough;
    }

    Plan::Resize {
        width: target_width,
    }
}

#[cfg(test)]
mod tests {
    use super::{AUTO_WIDTH, MIN_EDGE, Plan, plan};
    use crate::page::height_for_width;

    #[test]
    fn the_defaults_match_the_reference_tool() {
        assert_eq!(AUTO_WIDTH, 1280);
        assert_eq!(MIN_EDGE, 250);
    }

    #[test]
    fn a_page_wider_than_the_target_is_normalised() {
        assert_eq!(plan(1520, 2150, AUTO_WIDTH), Plan::Resize { width: 1280 });
        // The height the resampler will derive.
        assert_eq!(height_for_width(1520, 2150, 1280), 1811);
    }

    #[test]
    fn the_target_width_is_overridable() {
        assert_eq!(plan(1520, 2150, 1000), Plan::Resize { width: 1000 });
        assert_eq!(height_for_width(1520, 2150, 1000), 1414);
    }

    #[test]
    fn a_page_at_or_below_the_target_passes_through() {
        assert_eq!(plan(1000, 1400, AUTO_WIDTH), Plan::PassThrough);
        assert_eq!(plan(1280, 1811, AUTO_WIDTH), Plan::PassThrough);
        // One pixel wider is the first width that is reduced.
        assert_eq!(plan(1281, 1811, AUTO_WIDTH), Plan::Resize { width: 1280 });
    }

    #[test]
    fn a_result_below_the_minimum_edge_passes_through() {
        // A 30000x100 strip normalised to 1280 would be 1280x4.
        assert_eq!(height_for_width(30000, 100, 1280), 4);
        assert_eq!(plan(30000, 100, AUTO_WIDTH), Plan::PassThrough);
    }

    #[test]
    fn the_floor_is_symmetric_under_transposed_orientation() {
        // Each pair produces the same smaller *output* edge, one landscape and one
        // portrait, so both must take the same decision. Go's `reW <= 500 || reH <= 100`
        // could not: with a 1200x200 output it resized (1200 > 500, 200 > 100), and with
        // the transposed 200x1200 it passed through (200 <= 500). One threshold on the
        // smaller edge cannot disagree with itself.
        for (smaller, expected) in [
            (300, Plan::Resize { width: 1200 }),
            (200, Plan::PassThrough),
        ] {
            // Landscape: a 2400-wide source normalised to 1200.
            let landscape = plan(2400, smaller * 2, 1200);
            assert_eq!(height_for_width(2400, smaller * 2, 1200), smaller);

            // Portrait: the transpose, normalised to the same smaller edge.
            let portrait = plan(smaller * 2, 2400, smaller);
            assert_eq!(height_for_width(smaller * 2, 2400, smaller), 1200);

            assert_eq!(
                landscape, expected,
                "landscape with smaller output edge {smaller}"
            );
            assert_eq!(
                portrait,
                match expected {
                    Plan::Resize { .. } => Plan::Resize { width: smaller },
                    Plan::PassThrough => Plan::PassThrough,
                },
                "portrait with smaller output edge {smaller}"
            );
        }

        // The floor itself is inclusive: exactly 250 resizes, 249 does not.
        assert_eq!(height_for_width(1200, 300, 1000), 250);
        assert_eq!(plan(1200, 300, 1000), Plan::Resize { width: 1000 });
        assert_eq!(height_for_width(1200, 299, 1000), 249);
        assert_eq!(plan(1200, 299, 1000), Plan::PassThrough);
    }

    #[test]
    fn the_scale_target_carries_both_axes_only_when_resizing() {
        let resize = plan(1520, 2150, AUTO_WIDTH);
        assert_eq!(resize.scale_to(1520, 2150), Some((1280, 1811)));

        let through = plan(1000, 1400, AUTO_WIDTH);
        assert_eq!(through.scale_to(1000, 1400), None);
    }

    #[test]
    fn a_zero_axis_passes_through() {
        assert_eq!(plan(0, 100, AUTO_WIDTH), Plan::PassThrough);
        assert_eq!(plan(100, 0, AUTO_WIDTH), Plan::PassThrough);
        assert_eq!(plan(100, 100, 0), Plan::PassThrough);
    }
}
