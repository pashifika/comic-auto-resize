//! png, bmp and webp decode, through `image`.
//!
//! One crate for three formats rather than three codec crates and a hand-written BMP reader.
//! BMP is the reason: it is a family of seven compression schemes and five header
//! generations, and owning that matrix is what the reference implementation does. Measured
//! against the 89-file BMP conformance suite, `image` reads 27 of the 27 files a reader must
//! read and refuses the rest naming the feature.
//!
//! None of the three can produce a reduced image on the way in, so unlike the JPEG path the
//! pixel buffer here is a function of the *source* dimensions. What they do offer is the
//! header read: each concrete decoder reports its dimensions before any pixel data is
//! touched, which is what keeps the budget's refusal ahead of the allocation.
//!
//! The concrete decoders are used rather than `ImageReader`, and for a reason beyond taste:
//! `ImageReader::into_decoder` returns an opaque `impl ImageDecoder`, and detecting an
//! animation needs `PngDecoder::is_apng` and `WebPDecoder::has_animation`, which are on the
//! concrete types. `image`'s own decode limits are left at `no_limits` — which is what
//! `PngDecoder::new` sets — because this crate's [`Budget`] is the limit, and two limits with
//! different numbers would make the refusing one a matter of which fired first.
//!
//! Metadata does not survive here either: `DynamicImage::from_decoder` asks for pixels, so an
//! ICC profile is read past and dropped, as it is on the JPEG path.

use std::io::Cursor;

use image::codecs::bmp::BmpDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{DynamicImage, ImageDecoder, ImageError};

use crate::page::{Budget, Channels, Format, PageError, PageErrorKind, PageImage};

use super::Decoded;

/// A png page.
///
/// # Errors
///
/// See [`super::decode`]; an APNG is refused as [`PageErrorKind::MultiFrame`].
pub fn png(name: &str, buffer: &[u8], budget: Budget) -> Result<Decoded, PageError> {
    read(name, Format::Png, open_png(name, buffer)?, budget)
}

/// The geometry a png's header declares.
///
/// # Errors
///
/// See [`super::header`].
pub fn png_header(name: &str, buffer: &[u8]) -> Result<(u32, u32), PageError> {
    Ok(open_png(name, buffer)?.dimensions())
}

/// A bmp page.
///
/// # Errors
///
/// See [`super::decode`]. A sub-format this build cannot read — an OS/2 v2 header, Huffman,
/// RLE24, `BI_JPEG`, `BI_PNG`, 64-bit channels — is refused with `image`'s own message, which
/// names the feature.
pub fn bmp(name: &str, buffer: &[u8], budget: Budget) -> Result<Decoded, PageError> {
    read(name, Format::Bmp, open_bmp(name, buffer)?, budget)
}

/// The geometry a bmp's header declares.
///
/// # Errors
///
/// See [`super::header`].
pub fn bmp_header(name: &str, buffer: &[u8]) -> Result<(u32, u32), PageError> {
    Ok(open_bmp(name, buffer)?.dimensions())
}

/// A webp page.
///
/// # Errors
///
/// See [`super::decode`]; an animated webp is refused as [`PageErrorKind::MultiFrame`].
pub fn webp(name: &str, buffer: &[u8], budget: Budget) -> Result<Decoded, PageError> {
    read(name, Format::WebP, open_webp(name, buffer)?, budget)
}

/// The geometry a webp's header declares.
///
/// # Errors
///
/// See [`super::header`].
pub fn webp_header(name: &str, buffer: &[u8]) -> Result<(u32, u32), PageError> {
    Ok(open_webp(name, buffer)?.dimensions())
}

/// A png decoder over `buffer`, refusing an APNG.
fn open_png<'a>(name: &str, buffer: &'a [u8]) -> Result<PngDecoder<Cursor<&'a [u8]>>, PageError> {
    let decoder =
        PngDecoder::new(Cursor::new(buffer)).map_err(|error| failed(name, Format::Png, &error))?;
    if decoder
        .is_apng()
        .map_err(|error| failed(name, Format::Png, &error))?
    {
        return Err(multi_frame(name, Format::Png));
    }
    Ok(decoder)
}

/// A bmp decoder over `buffer`. BMP has no multi-frame form to refuse.
fn open_bmp<'a>(name: &str, buffer: &'a [u8]) -> Result<BmpDecoder<Cursor<&'a [u8]>>, PageError> {
    BmpDecoder::new(Cursor::new(buffer)).map_err(|error| failed(name, Format::Bmp, &error))
}

/// A webp decoder over `buffer`, refusing an animation.
fn open_webp<'a>(name: &str, buffer: &'a [u8]) -> Result<WebPDecoder<Cursor<&'a [u8]>>, PageError> {
    let decoder = WebPDecoder::new(Cursor::new(buffer))
        .map_err(|error| failed(name, Format::WebP, &error))?;
    if decoder.has_animation() {
        return Err(multi_frame(name, Format::WebP));
    }
    Ok(decoder)
}

/// Refuse from the header, decode, narrow to what the encoder takes.
fn read<D: ImageDecoder>(
    name: &str,
    format: Format,
    decoder: D,
    budget: Budget,
) -> Result<Decoded, PageError> {
    let (width, height) = decoder.dimensions();
    // From the header the decoder has already parsed, and before anything is allocated from
    // it — the same point in the sequence at which the JPEG path checks, reached deliberately
    // rather than as a by-product of choosing a scale.
    budget
        .allow_source(width, height)
        .map_err(|kind| PageError::new(name, kind))?;
    // The buffer the *decoder* will allocate, which is wider than the page that comes out of
    // it whenever there is an alpha channel or a 16-bit sample — eight bytes a pixel for
    // `Rgba16` against the page's three. Bounding this bounds the page too, because narrowing
    // only ever drops bytes.
    budget
        .allow_decoded(decoder.total_bytes())
        .map_err(|kind| PageError::new(name, kind))?;

    let image =
        DynamicImage::from_decoder(decoder).map_err(|error| failed(name, format, &error))?;
    let narrowed = narrow(image)
        .map_err(|shape| PageError::new(name, PageErrorKind::Pixels { format, shape }))?;
    let page = PageImage::new(width, height, narrowed.channels, narrowed.pixels)
        .map_err(|error| PageError::new(name, error.into()))?;
    Ok(Decoded {
        page,
        composited: narrowed.composited,
    })
}

/// What one of `image`'s buffers becomes on the way to a [`PageImage`].
struct Narrowed {
    channels: Channels,
    pixels: Vec<u8>,
    composited: bool,
}

impl Narrowed {
    /// A buffer that carried no alpha channel.
    fn opaque(channels: Channels, pixels: Vec<u8>) -> Self {
        Self {
            channels,
            pixels,
            composited: false,
        }
    }

    /// A buffer whose alpha channel was composited onto white, which the run counts.
    fn composited(channels: Channels, pixels: Vec<u8>) -> Self {
        Self {
            channels,
            pixels,
            composited: true,
        }
    }
}

/// Narrows whatever the decoder produced to the channel set the encoder takes, or names the
/// shape it could not.
///
/// Two rules, both stated in the requirement rather than left to a cast:
///
/// - **An alpha channel is composited onto white**, because a comic page's ground is paper
///   and refusing the page would refuse one a viewer displays correctly. Counted, because it
///   changes what the page looks like.
/// - **A sample deeper than eight bits is narrowed**, and not counted: every JPEG encoder
///   does this to every deeper source, so reporting it would report a property of the output
///   format rather than a decision taken about the page.
///
/// A single-component source stays single-component, as it does on the JPEG path: widening
/// grey to RGB triples every buffer and enlarges the output.
///
/// Measured, png arrives here as `L8`, `Rgba8` and `Rgba16`, and bmp and webp as `Rgb8` and
/// `Rgba8`. The float variants are unreachable from these three formats, and are refused by
/// name rather than converted on a guess about which transfer function they carry.
fn narrow(image: DynamicImage) -> Result<Narrowed, &'static str> {
    Ok(match image {
        // Already what the encoder takes, so the buffer moves through without a copy.
        DynamicImage::ImageLuma8(buffer) => Narrowed::opaque(Channels::Gray, buffer.into_raw()),
        DynamicImage::ImageRgb8(buffer) => Narrowed::opaque(Channels::Rgb, buffer.into_raw()),
        DynamicImage::ImageLuma16(buffer) => {
            Narrowed::opaque(Channels::Gray, narrowed(&buffer.into_raw()))
        }
        DynamicImage::ImageRgb16(buffer) => {
            Narrowed::opaque(Channels::Rgb, narrowed(&buffer.into_raw()))
        }
        DynamicImage::ImageLumaA8(buffer) => {
            Narrowed::composited(Channels::Gray, composited8(&buffer.into_raw(), 1))
        }
        DynamicImage::ImageRgba8(buffer) => {
            Narrowed::composited(Channels::Rgb, composited8(&buffer.into_raw(), 3))
        }
        DynamicImage::ImageLumaA16(buffer) => {
            Narrowed::composited(Channels::Gray, composited16(&buffer.into_raw(), 1))
        }
        DynamicImage::ImageRgba16(buffer) => {
            Narrowed::composited(Channels::Rgb, composited16(&buffer.into_raw(), 3))
        }
        DynamicImage::ImageRgb32F(_) => return Err("32-bit float RGB"),
        DynamicImage::ImageRgba32F(_) => return Err("32-bit float RGB with alpha"),
        // `DynamicImage` is `#[non_exhaustive]`, so a variant `image` adds later arrives here
        // instead of failing the build. Refused by name, which is the safe direction.
        _ => return Err("a pixel shape this build does not recognise"),
    })
}

/// Every 16-bit sample as eight bits.
fn narrowed(samples: &[u16]) -> Vec<u8> {
    samples.iter().copied().map(narrow_sample).collect()
}

/// An eight-bit buffer of `colour` colour channels plus one alpha, composited onto white with
/// the alpha channel dropped.
fn composited8(samples: &[u8], colour: usize) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(samples.len() / (colour + 1) * colour);
    for pixel in samples.chunks_exact(colour + 1) {
        let (colours, alpha) = pixel.split_at(colour);
        let alpha = alpha[0];
        pixels.extend(colours.iter().map(|&sample| over_white8(sample, alpha)));
    }
    pixels
}

/// The same over a 16-bit buffer, composited at the source's depth and then narrowed.
fn composited16(samples: &[u16], colour: usize) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(samples.len() / (colour + 1) * colour);
    for pixel in samples.chunks_exact(colour + 1) {
        let (colours, alpha) = pixel.split_at(colour);
        let alpha = alpha[0];
        pixels.extend(colours.iter().map(|&sample| over_white16(sample, alpha)));
    }
    pixels
}

/// A 16-bit sample as eight bits: its high byte.
///
/// One rule for every 16-bit source, so a page's untouched and composited regions narrow the
/// same way. `0xFFFF` maps to `0xFF`, `0x0000` to `0x00`, and the mapping is monotone.
fn narrow_sample(sample: u16) -> u8 {
    sample.to_be_bytes()[0]
}

/// One eight-bit colour sample composited over an opaque white ground.
///
/// `(sample * alpha + 255 * (255 - alpha) + 127) / 255`, which is the source over white with
/// the alpha channel dropped. **Exact at `alpha == 255`**: a fully opaque channel leaves the
/// colour value untouched, which is the case that would break silently if the arithmetic were
/// wrong — every pixel of every opaque-alpha page would shift by a little and nothing would
/// say so.
fn over_white8(sample: u8, alpha: u8) -> u8 {
    let sample = u32::from(sample);
    let alpha = u32::from(alpha);
    let blended = (sample * alpha + 255 * (255 - alpha) + 127) / 255;
    // At most 255 by construction; the saturating fallback keeps a nonsense value out of the
    // buffer rather than panicking, as `dimension` does on the JPEG path.
    u8::try_from(blended).unwrap_or(u8::MAX)
}

/// The same at sixteen bits, narrowed once at the end.
///
/// Composited at the source's own depth rather than after narrowing, so the result is rounded
/// once. `65535 * 65535 + 32767` is 4,294,868,992, which `u32` holds.
fn over_white16(sample: u16, alpha: u16) -> u8 {
    let sample = u32::from(sample);
    let alpha = u32::from(alpha);
    let blended = (sample * alpha + 65535 * (65535 - alpha) + 32767) / 65535;
    narrow_sample(u16::try_from(blended).unwrap_or(u16::MAX))
}

/// One of `image`'s errors as a [`PageErrorKind`].
///
/// The message is `image`'s own, deliberately: for a sub-format this build cannot read it
/// already names the feature — "does not support the format features JPEG compression",
/// "Palette size 300 exceeds maximum size for BMP", "Invalid channel bit count for RGB: 64" —
/// and naming the feature is what the refusal has to do rather than reporting that the entry
/// was not an image.
fn failed(name: &str, format: Format, error: &ImageError) -> PageError {
    PageError::new(
        name,
        PageErrorKind::Decode {
            format,
            reason: error.to_string(),
        },
    )
}

/// An animation refused rather than reduced to its first frame.
fn multi_frame(name: &str, format: Format) -> PageError {
    PageError::new(name, PageErrorKind::MultiFrame { format })
}

#[cfg(test)]
mod tests {
    use super::{composited8, composited16, narrow_sample, over_white8, over_white16};

    /// The case the composite exists to get right, and the one that would break silently: an
    /// alpha channel that is entirely opaque must leave every colour value where it was.
    #[test]
    fn a_fully_opaque_alpha_channel_leaves_the_colour_untouched() {
        for sample in 0..=u8::MAX {
            assert_eq!(over_white8(sample, u8::MAX), sample, "{sample}");
        }
        // And at sixteen bits, where the answer is the narrowing and nothing else.
        for sample in [0, 1, 0x0100, 0x7FFF, 0xFF00, 0xFFFF] {
            assert_eq!(
                over_white16(sample, u16::MAX),
                narrow_sample(sample),
                "{sample:#06X}"
            );
        }
    }

    #[test]
    fn a_fully_transparent_pixel_becomes_white() {
        for sample in [0, 1, 0x7F, 0xFF] {
            assert_eq!(over_white8(sample, 0), u8::MAX);
        }
        assert_eq!(over_white16(0, 0), u8::MAX);
        assert_eq!(over_white16(u16::MAX, 0), u8::MAX);
    }

    /// Halfway is halfway towards white, not towards black and not left alone. Black at
    /// `alpha = 128` is `255 * (1 - 128/255)`, which rounds to 127 rather than to 128 —
    /// asserted at the value rather than approximately, so a change to the rounding is
    /// visible.
    #[test]
    fn a_half_transparent_pixel_moves_towards_white() {
        assert_eq!(over_white8(0, 128), 127);
        assert_eq!(over_white8(u8::MAX, 128), u8::MAX);
        // A mid-grey over white at half alpha lands between the two.
        let grey = over_white8(0x40, 128);
        assert!(grey > 0x40 && grey < 0xFF, "{grey:#04X}");
    }

    #[test]
    fn compositing_drops_the_alpha_channel_and_keeps_the_colour_channels() {
        // Two RGBA pixels: opaque black, then transparent black.
        let samples = [0, 0, 0, 0xFF, 0, 0, 0, 0x00];
        assert_eq!(composited8(&samples, 3), [0, 0, 0, 0xFF, 0xFF, 0xFF]);

        // Two grey-alpha pixels, same shape.
        let grey = [0x40, 0xFF, 0x40, 0x00];
        assert_eq!(composited8(&grey, 1), [0x40, 0xFF]);
    }

    #[test]
    fn a_sixteen_bit_composite_narrows_to_eight() {
        let samples = [0x4080, 0x8000, 0xC000, 0xFFFF];
        assert_eq!(composited16(&samples, 3), [0x40, 0x80, 0xC0]);
    }

    #[test]
    fn narrowing_keeps_the_high_byte() {
        assert_eq!(narrow_sample(0), 0);
        assert_eq!(narrow_sample(0x00FF), 0);
        assert_eq!(narrow_sample(0x0100), 1);
        assert_eq!(narrow_sample(u16::MAX), u8::MAX);
    }
}
