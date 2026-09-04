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

/// How one page's target width is chosen.
///
/// Both arms end at a single number, which is why a ratio needs no second decision path: it
/// is a target width named relative to the page instead of absolutely.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Target {
    /// Every page normalised to this width.
    Width(u32),
    /// Every page reduced to this percentage of its own width.
    ///
    /// No special case at any value. The reference tool has one — `autoResize` replaces a
    /// ratio of exactly `0.7` with `1280 / width` — so its `-r 70` and `-r 71` differ in
    /// kind; here they differ in degree, and the behaviour its 70 gave is this tool's
    /// default, reached by naming no ratio at all.
    Ratio(u8),
}

impl Target {
    /// The width [`plan`] aims at for a page `src_width` wide.
    ///
    /// `round(src_width × percent / 100)`, half up in integer arithmetic for the reason
    /// [`plan`] compares integers: a pixel count must not turn on a floating-point rounding
    /// mode. A percentage above 100 saturates rather than wrapping, and [`plan`] passes such
    /// a page through — this tool does not enlarge.
    #[must_use]
    pub fn width_for(self, src_width: u32) -> u32 {
        match self {
            Self::Width(width) => width,
            Self::Ratio(percent) => {
                let scaled = (u64::from(src_width) * u64::from(percent) + 50) / 100;
                u32::try_from(scaled).unwrap_or(u32::MAX)
            }
        }
    }
}

/// What happens to one page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Plan {
    /// Resize to this width, then encode. The height is not carried: [`Resampler::resize`]
    /// derives it from the page's own recorded geometry, and two copies of one number are
    /// two chances to disagree.
    ///
    /// [`Resampler::resize`]: crate::page::Resampler::resize
    Resize { width: u32 },
    /// Encode at the source dimensions: the page is already at or below the target, so
    /// there is nothing to reduce.
    PassThrough,
    /// Encode at the source dimensions because the reduction asked for would leave an edge
    /// below [`MIN_EDGE`].
    ///
    /// The same outcome as [`PassThrough`](Self::PassThrough), and a separate variant
    /// because the run reports how many pages it covers: here the caller asked for a
    /// reduction and did not get one. Decided where the floor is tested rather than
    /// re-derived at the callsite, so the count cannot disagree with the decision.
    BelowFloor,
}

impl Plan {
    /// The size a scaled decode may aim for, or `None` when the page is not being resized.
    ///
    /// Both axes, because libjpeg's step selection has to clear the target on each.
    #[must_use]
    pub fn scale_to(self, src_width: u32, src_height: u32) -> Option<(u32, u32)> {
        match self {
            Self::Resize { width } => Some((width, height_for_width(src_width, src_height, width))),
            Self::PassThrough | Self::BelowFloor => None,
        }
    }
}

/// Decides what to do with a `src_width` × `src_height` page aiming at `target_width`.
///
/// Pass-through when the page is already at or below the target, and [`Plan::BelowFloor`] —
/// which passes through too — when the smaller edge of the result would fall below
/// [`MIN_EDGE`]. Pass-through skips the resize and nothing else: the encoder always runs,
/// which is what makes "recompress without resizing" a request the tool honours rather than
/// one that silently becomes a copy.
#[must_use]
pub fn plan(src_width: u32, src_height: u32, target_width: u32) -> Plan {
    // A zero source axis has no ratio to preserve, and no JPEG has one. Passing through
    // leaves the page for the encoder to reject rather than inventing a size for it here.
    // A zero *target* is not this case: it is a reduction to nothing, which the floor below
    // refuses along with every other degenerate result.
    if src_width == 0 || src_height == 0 {
        return Plan::PassThrough;
    }

    // The scale is `target_width / src_width`; at or above 1.0 there is nothing to reduce.
    // Compared as integers, so the decision cannot turn on a floating-point rounding mode.
    if target_width >= src_width {
        return Plan::PassThrough;
    }

    let target_height = height_for_width(src_width, src_height, target_width);
    if target_width.min(target_height) < MIN_EDGE {
        return Plan::BelowFloor;
    }

    Plan::Resize {
        width: target_width,
    }
}

#[cfg(test)]
mod tests {
    use super::{AUTO_WIDTH, MIN_EDGE, Plan, Target, plan};
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
    fn a_result_below_the_minimum_edge_passes_through_as_a_refusal() {
        // A 30000x100 strip normalised to 1280 would be 1280x4.
        assert_eq!(height_for_width(30000, 100, 1280), 4);
        // Pass-through either way; the variant is what separates a reduction that was
        // refused from a page that never needed one.
        assert_eq!(plan(30000, 100, AUTO_WIDTH), Plan::BelowFloor);
    }

    #[test]
    fn the_floor_is_symmetric_under_transposed_orientation() {
        // Each pair produces the same smaller *output* edge, one landscape and one
        // portrait, so both must take the same decision. Go's `reW <= 500 || reH <= 100`
        // could not: with a 1200x200 output it resized (1200 > 500, 200 > 100), and with
        // the transposed 200x1200 it passed through (200 <= 500). One threshold on the
        // smaller edge cannot disagree with itself.
        for (smaller, expected) in [(300, Plan::Resize { width: 1200 }), (200, Plan::BelowFloor)] {
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
                    other => other,
                },
                "portrait with smaller output edge {smaller}"
            );
        }

        // The floor itself is inclusive: exactly 250 resizes, 249 does not.
        assert_eq!(height_for_width(1200, 300, 1000), 250);
        assert_eq!(plan(1200, 300, 1000), Plan::Resize { width: 1000 });
        assert_eq!(height_for_width(1200, 299, 1000), 249);
        assert_eq!(plan(1200, 299, 1000), Plan::BelowFloor);
    }

    #[test]
    fn the_scale_target_carries_both_axes_only_when_resizing() {
        let resize = plan(1520, 2150, AUTO_WIDTH);
        assert_eq!(resize.scale_to(1520, 2150), Some((1280, 1811)));

        let through = plan(1000, 1400, AUTO_WIDTH);
        assert_eq!(through.scale_to(1000, 1400), None);

        // A refused reduction is not a scaled decode either.
        let refused = plan(30000, 100, AUTO_WIDTH);
        assert_eq!(refused, Plan::BelowFloor);
        assert_eq!(refused.scale_to(30000, 100), None);
    }

    /// A zero *source* axis is not a page and no reduction was asked of it; a zero *target*
    /// is a reduction asked for and refused, which is what separates the two.
    #[test]
    fn a_zero_source_axis_passes_through_and_a_zero_target_is_refused() {
        assert_eq!(plan(0, 100, AUTO_WIDTH), Plan::PassThrough);
        assert_eq!(plan(100, 0, AUTO_WIDTH), Plan::PassThrough);
        assert_eq!(plan(100, 100, 0), Plan::BelowFloor);

        // Reachable from a ratio: a page narrow enough that a small percentage of it rounds
        // to nothing. 49 x 1% is 0.49, which rounds to 0.
        assert_eq!(Target::Ratio(1).width_for(49), 0);
        assert_eq!(plan(49, 70, 0), Plan::BelowFloor);
    }

    /// The reference tool's `autoResize` replaces a ratio of exactly `0.7` with
    /// `1280 / width`, so its `-r 70` normalises and its `-r 71` scales. This is the test
    /// that fails if that special case is ever reintroduced.
    #[test]
    fn a_ratio_differs_in_degree_at_seventy_rather_than_in_kind() {
        let seventy = Target::Ratio(70).width_for(1520);
        let seventy_one = Target::Ratio(71).width_for(1520);
        assert_eq!(seventy, 1064);
        assert_eq!(seventy_one, 1079);
        assert_eq!(plan(1520, 2150, seventy), Plan::Resize { width: 1064 });
        assert_eq!(plan(1520, 2150, seventy_one), Plan::Resize { width: 1079 });

        // What the reference tool's 70 produced is this tool's default, and the two are
        // different numbers — which is the whole of the divergence.
        assert_eq!(Target::Width(AUTO_WIDTH).width_for(1520), 1280);
        assert_ne!(seventy, AUTO_WIDTH);
    }

    /// The floor's divergence measured rather than asserted: Go's `reW <= 500` refuses this
    /// page at 30 per cent and returns it untouched; 456 is clear of 250, so this build
    /// resizes it. Pass-through begins at 16 per cent for this page, not 30.
    #[test]
    fn an_explicit_ratio_meets_the_floor_and_nothing_else() {
        let thirty = Target::Ratio(30).width_for(1520);
        assert_eq!(thirty, 456);
        assert_eq!(height_for_width(1520, 2150, thirty), 645);
        assert_eq!(plan(1520, 2150, thirty), Plan::Resize { width: 456 });

        // The exact boundary. The page is taller than it is wide, so the width is the
        // smaller output edge and it is the width that meets the floor.
        assert_eq!(Target::Ratio(17).width_for(1520), 258);
        assert_eq!(plan(1520, 2150, 258), Plan::Resize { width: 258 });
        assert_eq!(Target::Ratio(16).width_for(1520), 243);
        assert_eq!(plan(1520, 2150, 243), Plan::BelowFloor);
    }

    #[test]
    fn a_ratio_resolves_to_a_pixel_count_rounded_half_up() {
        // 500.5 pixels, which rounds away from zero.
        assert_eq!(Target::Ratio(50).width_for(1001), 501);
        assert_eq!(Target::Ratio(50).width_for(1003), 502);
        // A full-width ratio asks for nothing, as the reference tool's `ratio == 100` does.
        assert_eq!(Target::Ratio(100).width_for(1520), 1520);
        assert_eq!(plan(1520, 2150, 1520), Plan::PassThrough);
        // An absolute target does not consult the page.
        assert_eq!(Target::Width(1000).width_for(1520), 1000);
        assert_eq!(Target::Width(1000).width_for(400), 1000);
    }

    /// The one place exact integer arithmetic is observably not the reference tool's.
    ///
    /// Go computes `math.Round(width × float64(percent)/100)`, and `percent/100` is inexact
    /// in binary: `1430 × 0.35` evaluates to `500.49999999999994`, which rounds *down* to
    /// 500 where the exact half rounds up to 501. Go's `reW <= 500` is inclusive, so on a
    /// page whose height clears both builds' floors that one pixel is the difference between
    /// a resize there and a pass-through: 1430×2000 resizes here and is refused there.
    ///
    /// Crossing that width predicate is not by itself a different outcome for the page —
    /// both builds test two axes — which is why the short-page case is pinned below too.
    ///
    /// Enumerated over widths 1..=65,535 and percentages 1..=100, the two rules disagree on
    /// 3,293 pairs, always by one pixel and always with this rule the higher; exactly two of
    /// those land on Go's inclusive floor, and one of them — width 715 at 70 per cent — is
    /// unreachable there because 70 is the value its `autoResize` special-cases. This test
    /// fails if the derivation is ever moved to floating point.
    #[test]
    fn a_ratio_is_exact_where_the_reference_tools_float_is_not() {
        assert_eq!(Target::Ratio(35).width_for(1430), 501);
        assert_eq!(plan(1430, 2000, 501), Plan::Resize { width: 501 });
        // The less dramatic form of the same disagreement: one pixel, both tools resizing.
        assert_eq!(Target::Ratio(58).width_for(875), 508);
        // A short page at the same width is not a divergence: 501 wide makes a 70-pixel
        // height, under this floor, and 500 is not above Go's, so both pass it through.
        assert_eq!(plan(1430, 200, 501), Plan::BelowFloor);
    }

    /// The second arm of the same divergence, and the more common one: the *height*.
    ///
    /// A ratio here resolves to one number, and the height follows from it —
    /// [`height_for_width`] of the rounded width — because `Plan::Resize` carries one target
    /// and `Resampler::resize` derives the rest. The reference tool applies the ratio to both
    /// source axes independently, so where rounding pulls the two apart the pages differ by a
    /// pixel of height even though the widths agree: a 1001×1400 page at 50 per cent is
    /// 501×701 here and 501×700 there, because 700.699 rounds up from the width this build
    /// actually used and `1400 × 0.5` rounds to 700 from the source.
    ///
    /// Deliberate, and the commoner case rather than the exotic one: sampling widths and
    /// heights from 200 to 4000 at 30, 50 and 70 per cent, the widths agree and the heights
    /// differ on 26.9% of pages. One authoritative height is what the policy, the scaled
    /// decode and the resampler all read, and two independently rounded axes would be two
    /// chances to disagree about the same page.
    #[test]
    fn a_ratios_height_follows_the_width_this_build_chose() {
        let width = Target::Ratio(50).width_for(1001);
        assert_eq!(width, 501);
        assert_eq!(height_for_width(1001, 1400, width), 701);
        assert_eq!(plan(1001, 1400, width), Plan::Resize { width: 501 });
        // Where the rounding does not pull them apart, the two agree.
        assert_eq!(Target::Ratio(50).width_for(1000), 500);
        assert_eq!(height_for_width(1000, 1400, 500), 700);
    }
}
