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
//! concrete types.
//!
//! # The png decoder is given a limit of its own, and that is not a second budget
//!
//! An earlier version left `image`'s limits at `no_limits` — which is what `PngDecoder::new`
//! sets — on the grounds that [`Budget`] is the limit. An independent review found the hole
//! that reasoning left: `png`'s `read_info` parses every chunk before the first `IDAT`, and
//! `parse_iccp_raw` inflates an `iCCP` profile with `fdeflate::decompress_to_vec_bounded(buf,
//! self.limits.bytes)` — which `no_limits` makes `usize::MAX`. So a png declaring `1x1` and
//! carrying a compressed run of identical bytes inflates without bound *during header
//! parsing*, before `dimensions()` is readable and therefore before any budget check. Measured
//! at zlib level 9 the ratio reaches 1028:1, so a 65 KB chunk becomes 64 MiB and an entry at
//! `MAX_ENTRY_BYTES` becomes tens of gigabytes. `Vec` growth ends in `handle_alloc_error`,
//! which **aborts** — `catch_unwind` in the pipeline cannot intercept it.
//!
//! [`Budget`] cannot close that, because the inflated size is not a pixel count. So the decoder
//! is constructed with a pool of its own, sized by [`png_pool`] from the **entry's** length
//! rather than from a constant — the budget's page ceiling is only a cap on it and never binds
//! outside a test that lowers it. Measured on the release binary, that took the attack above
//! from 13.86 GB of peak RSS to 24.8 MB.
//!
//! **What it costs**, because a pool is not free: a png whose ancillary chunks charge more than
//! the pool holds is refused, and `png` charges a chunk twice — see [`png_pool`] for the
//! arithmetic and for the legal page a first draft of it refused. The refusal a reader might
//! expect instead does *not* happen: `parse_iccp` discards its own error
//! (`stream.rs:1696`, `let _ = self.parse_iccp_raw();`), so an over-large profile is silently
//! dropped and the page decodes. That is the right outcome here — the profile is discarded
//! anyway — but it means the pool is invisible on the path it was added for, which is why the
//! regression test asserts it through the chunk-growth refusal instead.
//!
//! bmp and webp need no equivalent: `image-webp` reads a profile only through `icc_profile()`,
//! which `from_decoder` never calls, and bmp has no compressed ancillary chunk at all.
//!
//! Metadata still does not survive: `DynamicImage::from_decoder` asks for pixels, so a profile
//! that *is* inflated is then dropped, as it is on the JPEG path. The limit exists because the
//! inflation happens whether or not anyone wants the result.

use std::io::Cursor;

use image::codecs::bmp::BmpDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{ColorType, DynamicImage, ImageDecoder, ImageError, Limits};

use crate::page::{Budget, Channels, Format, PageError, PageErrorKind, PageImage};

use super::Decoded;

/// A png page.
///
/// # Errors
///
/// See [`super::decode`]; an APNG is refused as [`PageErrorKind::MultiFrame`].
pub fn png(name: &str, buffer: &[u8], budget: Budget) -> Result<Decoded, PageError> {
    read(name, Format::Png, open_png(name, buffer, budget)?, budget)
}

/// The geometry a png's header declares.
///
/// Takes the budget because reading a png's header is where the decoder inflates an `iCCP`
/// chunk; see the module documentation.
///
/// # Errors
///
/// See [`super::header`].
pub fn png_header(name: &str, buffer: &[u8], budget: Budget) -> Result<(u32, u32), PageError> {
    Ok(open_png(name, buffer, budget)?.dimensions())
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

/// Room for one output scanline in the png decoder's pool.
///
/// An **allowance, not a bound**: PNG's `IHDR` width is a 32-bit field and
/// `unguarded_output_line_size` is `width × samples × bytes_per_sample` with no cap
/// (`png-0.18.1/src/decoder/mod.rs:683-686`), so a png may ask for a scanline of gigabytes.
/// 1 MiB covers every page whose *pixel count* the budget admits at any sane aspect ratio; a
/// page wide enough to exceed it is refused, and [`png_geometry`] is what makes that refusal
/// the budget's rather than the decoder's.
const PNG_SCANLINE_ALLOWANCE: u64 = 1 << 20;

/// The most bytes `image`'s png decoder may draw for its **own** allocations while reading
/// `entry` bytes of png.
///
/// Not a second page budget, and the distinction is what makes this number safe. `png`'s
/// `Limits` is a *decrementing* pool (`decoder/mod.rs:71-78`) over the decoder's internal
/// allocations only — never the pixel buffer, which `image` allocates itself and [`Budget`]
/// bounds, and which `PngDecoder::read_image` does not charge (`codecs/png.rs:242-262`).
///
/// Three entries, because a chunk is charged **twice** and the first draft's arithmetic missed
/// it — an independent review found a legal 3.2 MB png with a 2 MiB `tEXt` chunk being refused:
///
/// - `2 × entry` bounds the capacity growth of the buffer holding the largest chunk. It doubles
///   from 128 and reserves each step (`stream.rs:782-792`), so reaching a capacity that holds
///   `L` bytes costs up to just under `2L`, and the capacity persists across chunks because
///   `raw_bytes.clear()` keeps it (`stream.rs:915`).
/// - `1 × entry` bounds the sum of the per-chunk charges `tEXt`, `zTXt` and `iTXt` add on top
///   (`stream.rs:1843,1858,1878`), because every chunk's bytes come out of the entry.
/// - `iCCP` needs no term: its inflate is bounded by whatever the pool has left
///   (`stream.rs:1726`), which is the whole point of giving the decoder a pool at all.
///
/// At `MAX_ENTRY_BYTES` that is 193 MiB, so the budget's page ceiling never binds outside a
/// test that lowers it. Sizing from the entry rather than from a constant is what keeps a 2 MB
/// entry from being allowed 64 MB of scratch: measured, an archive of eight pngs each declaring
/// `1x1` and carrying a profile that inflates to 2 GiB peaked at **13.86 GB** with no pool at
/// all, and 24.8 MB with this one.
fn png_pool(budget: Budget, entry: usize) -> u64 {
    let entry = u64::try_from(entry).unwrap_or(u64::MAX);
    entry
        .saturating_mul(3)
        .saturating_add(PNG_SCANLINE_ALLOWANCE)
        .min(budget.max_image_bytes())
}

/// The geometry a png's `IHDR` declares, read from the bytes rather than through a decoder.
///
/// Thirteen bytes at a fixed offset: eight of signature, then a four-byte length and the
/// four-byte type, so the width and height sit at 16 and 20. Read here because *constructing* a
/// decoder is itself an allocating operation — `read_info` reserves one output scanline against
/// the pool — so a page whose pixel count is the problem has to be refused before that, or the
/// refusal stops naming the quantity and the limit and says "Memory limit exceeded" instead.
///
/// `None` for bytes that are not a png at all, which the decoder then refuses by name.
fn png_geometry(buffer: &[u8]) -> Option<(u32, u32)> {
    if buffer.len() < 24 || &buffer[12..16] != b"IHDR" {
        return None;
    }
    Some((
        u32::from_be_bytes(buffer[16..20].try_into().ok()?),
        u32::from_be_bytes(buffer[20..24].try_into().ok()?),
    ))
}

/// A png decoder over `buffer`, refusing an APNG.
///
/// `with_limits` rather than `new`, and the module documentation says why: `new` is
/// `no_limits`, which lets an `iCCP` chunk inflate without bound during header parsing.
fn open_png<'a>(
    name: &str,
    buffer: &'a [u8],
    budget: Budget,
) -> Result<PngDecoder<Cursor<&'a [u8]>>, PageError> {
    // Before the decoder exists, so an oversized page is refused naming its pixel count rather
    // than by the decoder exhausting its own pool on a scanline.
    if let Some((width, height)) = png_geometry(buffer) {
        budget
            .allow_source(width, height)
            .map_err(|kind| PageError::new(name, kind))?;
    }

    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(png_pool(budget, buffer.len()));
    let decoder = PngDecoder::with_limits(Cursor::new(buffer), limits)
        .map_err(|error| failed(name, Format::Png, &error))?;
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
    let named = |kind| PageError::new(name, kind);

    let (width, height) = decoder.dimensions();
    // From the header the decoder has already parsed, and before anything is allocated from
    // it — the same point in the sequence at which the JPEG path checks, reached deliberately
    // rather than as a by-product of choosing a scale.
    budget.allow_source(width, height).map_err(named)?;

    // What the colour type narrows to, and what the decoder allocates around it, decided from
    // the header rather than after the decode. Two things follow from doing it here: a shape no
    // rule covers is refused without paying for its pixels, and the budget can charge the
    // *peak* rather than one buffer.
    let narrowing = Narrowing::of(format, decoder.color_type()).ok_or_else(|| {
        named(PageErrorKind::Pixels {
            format,
            shape: format!("{:?}", decoder.color_type()),
        })
    })?;
    budget
        .allow_decoded(narrowing.peak(decoder.total_bytes(), width, height))
        .map_err(named)?;

    let image =
        DynamicImage::from_decoder(decoder).map_err(|error| failed(name, format, &error))?;
    let narrowed = narrow(image).map_err(|shape| {
        named(PageErrorKind::Pixels {
            format,
            shape: shape.to_owned(),
        })
    })?;
    debug_assert_eq!(
        narrowed.channels, narrowing.channels,
        "the header-side and buffer-side narrowing tables disagree"
    );
    let page = PageImage::new(width, height, narrowed.channels, narrowed.pixels)
        .map_err(|error: crate::page::InvalidPixelBuffer| PageError::new(name, error.into()))?;
    Ok(Decoded {
        page,
        composited: narrowed.composited,
    })
}

/// What one decoder allocates for **itself**, beyond the buffer it declares.
///
/// `ImageDecoder::total_bytes()` is the buffer `image` allocates and hands to `read_image`
/// (`image-0.25.10/src/io/free_functions.rs:305-319` allocates exactly one), and a concrete
/// decoder may allocate more around it. That excess is sized by the input like everything
/// else, so the budget charges it: `declared × numerator / denominator`.
///
/// **The two webp arms are above one.** `image-webp`'s `read_image` allocates `w × h × 4` and
/// copies down when the colour type is `Rgb8` (`image-webp-0.2.4/src/decoder.rs:687-691`), so
/// three declared bytes a pixel cost seven — exactly `7/3`, held as a fraction rather than as a
/// rounded 2.33 because the charge is an allocation bound. Every png and bmp arm writes into
/// the buffer `image` gave it: png's `next_frame` fills it and byte-swaps in place, and bmp's
/// `read_image_data` decodes into it a row at a time.
///
/// The factor is per arm rather than global. A single worst-case factor would charge every png
/// the price of webp's scratch and refuse pages that do not incur it — the excess is
/// input-selected, so the charge has to be too.
///
/// # One arm is as fine-grained as the decoder lets it be
///
/// A webp is VP8 or VP8L and the two allocate differently, but `WebPDecoder` reports only a
/// `ColorType`, so `(WebP, Rgb8)` covers both codings and `(WebP, Rgba8)` covers both again.
/// Measured, bytes a pixel of total working set:
///
/// | arm | VP8L | VP8 | charged |
/// |---|---|---|---|
/// | `Rgb8` | 7.00 | 4.67 | `7/3` → 7.00 |
/// | `Rgba8` | 7.00 | 10.74 | `2/1` → 11.00 |
///
/// Each row charges the **larger** of the two codings, because the code cannot see which one it
/// has: a row charging the smaller would be a bound that does not hold. Reading the RIFF chunk's
/// four-character code would separate them, and it is not worth it — the two regions it would
/// recover are lossy `Rgb8` from 38.3 to 57.5 Mpx and lossless `Rgba8` from 24.4 to 38.3 Mpx,
/// which are pages six thousand pixels on a side, and it would add a second opinion about which
/// coding a buffer holds for `image-webp`'s to disagree with.
///
/// `Rgba8`'s `2/1` is the one factor that is not a ratio of two identified allocations: 1.9343
/// measured, rounded up to the next simple fraction. VP8 with alpha holds the 4-byte output
/// buffer, VP8's YUV planes at 1.5 B/px, and `read_alpha_chunk`'s own lossless decode at about
/// 5 B/px, and then `narrow` composites the page's three beside it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Scratch {
    format: Format,
    colour: ColorType,
    numerator: u64,
    denominator: u64,
    /// The dependency versions the figure was measured against.
    ///
    /// A scratch factor is a property of the decoder, so a bump that moves one has to be a
    /// re-measurement rather than a mystery. Recorded here for the reason
    /// `tests/fixtures/bmp-outcomes.tsv` records the conformance suite.
    against: &'static str,
}

/// The versions the png and bmp arms were measured against; both decode through `image` itself.
const PNG_BMP: &str = "image 0.25.10, png 0.18.1";

/// The versions the webp arms were measured against.
const WEBP: &str = "image 0.25.10, image-webp 0.2.4";

/// Every arm these three decoders produce, with its measured factor.
///
/// Complete by construction rather than by hope: `every_arm_the_decoders_produce_is_measured`
/// walks the colour types [`Narrowing::of`] admits against the formats [`read`] is reached
/// with, and fails naming any pair this table does not carry. An arm that is nonetheless
/// absent is charged one and **says it is unmeasured** — see [`Narrowing::scratch`].
///
/// Method, because a factor without one is not a measurement: `page::decode` alone on a
/// five-point size ladder from 3.27 to 48 Mpx, peak resident set against page area, slope
/// after subtracting the entry buffer. The decode is isolated from the resampler deliberately.
/// A run's peak is the *maximum* over its stages, and for every arm at one the resampler's
/// two-pass buffer is the larger of the two — so a factor taken from the whole pipeline
/// attributes the resampler's buffer to the decoder. That is how an earlier survey read
/// webp's `7/3` as 2.72. Every measured slope came out within 0.1% of an exact integer count
/// of bytes a pixel.
///
/// Fourteen measurements behind twelve rows: the two webp rows each cover two codings, and each
/// charges the worse of its pair.
const SCRATCH: &[Scratch] = &[
    Scratch::new(Format::Png, ColorType::L8, 1, 1, PNG_BMP),
    Scratch::new(Format::Png, ColorType::La8, 1, 1, PNG_BMP),
    Scratch::new(Format::Png, ColorType::L16, 1, 1, PNG_BMP),
    Scratch::new(Format::Png, ColorType::La16, 1, 1, PNG_BMP),
    Scratch::new(Format::Png, ColorType::Rgb8, 1, 1, PNG_BMP),
    Scratch::new(Format::Png, ColorType::Rgba8, 1, 1, PNG_BMP),
    Scratch::new(Format::Png, ColorType::Rgb16, 1, 1, PNG_BMP),
    Scratch::new(Format::Png, ColorType::Rgba16, 1, 1, PNG_BMP),
    Scratch::new(Format::Bmp, ColorType::Rgb8, 1, 1, PNG_BMP),
    Scratch::new(Format::Bmp, ColorType::Rgba8, 1, 1, PNG_BMP),
    // The two arms that allocate around the buffer they were given, each charging the worse of
    // the two codings it cannot distinguish.
    Scratch::new(Format::WebP, ColorType::Rgb8, 7, 3, WEBP),
    Scratch::new(Format::WebP, ColorType::Rgba8, 2, 1, WEBP),
];

impl Scratch {
    const fn new(
        format: Format,
        colour: ColorType,
        numerator: u64,
        denominator: u64,
        against: &'static str,
    ) -> Self {
        Self {
            format,
            colour,
            numerator,
            denominator,
            against,
        }
    }

    /// The measured factor for one arm, or `None` where nobody has measured it.
    fn of(format: Format, colour: ColorType) -> Option<&'static Self> {
        SCRATCH
            .iter()
            .find(|arm| arm.format == format && arm.colour == colour)
    }

    /// `declared` times this factor, rounded **up**.
    ///
    /// Up rather than towards zero: the product bounds an allocation, and a bound that rounds
    /// down is a bound that is short. `u128` for the reason [`Budget::allow_decoded`] is —
    /// `declared` is already a `u64` a header chose.
    fn charge(&self, declared: u64) -> u128 {
        (u128::from(declared) * u128::from(self.numerator)).div_ceil(u128::from(self.denominator))
    }
}

/// What a decoder's colour type becomes, read off its header.
///
/// Separate from [`narrow`], which decides what to do with the *samples*: this decides only the
/// channel count, whether a second buffer is built, and what the decoder's own working set
/// costs — which is what the budget needs before a pixel exists.
///
/// `the_two_narrowing_tables_agree` cross-checks the **channel** half against a real [`narrow`]
/// call, so that half cannot drift. It cannot observe the `copies` half directly — no test can,
/// short of an instrumented allocator — so it compares it against its own declared list of the
/// moved arms. A change to `narrow` that composited in place, or that cloned an `Rgb8` buffer,
/// would make `peak` charge wrongly and the test would still pass.
struct Narrowing {
    channels: Channels,
    /// Whether [`narrow`] builds a new buffer while the decoder's is still alive.
    ///
    /// `L8` and `Rgb8` are already what the encoder takes, so their buffer is **moved** and
    /// there is never a second allocation. Every other arm composites or narrows into a fresh
    /// buffer with the source still held, so both are alive at once.
    copies: bool,
    /// What the decoder allocates for itself, or `None` for an arm with no measurement.
    ///
    /// `None` is charged as one — the declared buffer and nothing more — and is *reported*
    /// rather than implied. A factor table is exactly the shape that invites a guess dressed as
    /// a bound, so an arm nobody has measured says so instead of carrying a plausible number.
    scratch: Option<&'static Scratch>,
}

impl Narrowing {
    /// The narrowing `colour` takes for a page `format` selected, or `None` for a shape no rule
    /// covers.
    ///
    /// `format` chooses the scratch factor and nothing else: the channel count and the copy are
    /// properties of the colour type alone.
    fn of(format: Format, colour: ColorType) -> Option<Self> {
        let (channels, copies) = match colour {
            ColorType::L8 => (Channels::Gray, false),
            ColorType::Rgb8 => (Channels::Rgb, false),
            ColorType::La8 | ColorType::L16 | ColorType::La16 => (Channels::Gray, true),
            ColorType::Rgba8 | ColorType::Rgb16 | ColorType::Rgba16 => (Channels::Rgb, true),
            // `Rgb32F`, `Rgba32F`, and anything `image` adds to a `#[non_exhaustive]` enum
            // later. Refused by name rather than converted on a guess about which transfer
            // function the samples carry.
            _ => return None,
        };
        Some(Self {
            channels,
            copies,
            scratch: Scratch::of(format, colour),
        })
    }

    /// The most bytes alive at once for a `width` × `height` page whose decoder asks for
    /// `decoded`.
    ///
    /// Three terms, and each is a measurement rather than a reading of the code. `decoded` is
    /// the decoder's own figure — eight bytes a pixel for `Rgba16` against the page's three —
    /// times the factor its own allocations add. The page's buffer is added only where the two
    /// coexist.
    fn peak(&self, decoded: u64, width: u32, height: u32) -> u128 {
        let page = if self.copies {
            u128::from(width) * u128::from(height) * u128::from(self.channels.count())
        } else {
            0
        };
        self.scratch
            .map_or_else(|| u128::from(decoded), |scratch| scratch.charge(decoded))
            + page
    }
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
    use image::{ColorType, DynamicImage};

    use crate::page::{Budget, Format};

    use super::{
        Narrowing, SCRATCH, Scratch, composited8, composited16, narrow, narrow_sample, over_white8,
        over_white16,
    };

    /// The two tables that describe a narrowing cannot drift.
    ///
    /// [`Narrowing::of`] reads a decoder's colour type before any pixel exists, so the budget
    /// can charge the peak and a shape no rule covers can be refused without paying for its
    /// pixels. [`narrow`] does the samples. They are separate matches over two different enums,
    /// so this walks every `ColorType` `Narrowing::of` admits, builds the `DynamicImage` arm a
    /// decoder of that type produces, and asserts the pair agrees on the channel count and on
    /// whether a second buffer is built.
    ///
    /// `Format::Png` throughout, because png is the one format that produces all eight colour
    /// types and because every png arm's scratch factor is one — so the assertion below stays
    /// "the peak is the two buffers" and the factor is tested where it is not one.
    #[test]
    fn the_two_narrowing_tables_agree() {
        // 2x1, so a buffer's length distinguishes the channel count from the pixel count.
        let (width, height) = (2, 1);
        let arms = [
            (ColorType::L8, DynamicImage::new_luma8(width, height)),
            (ColorType::La8, DynamicImage::new_luma_a8(width, height)),
            (ColorType::Rgb8, DynamicImage::new_rgb8(width, height)),
            (ColorType::Rgba8, DynamicImage::new_rgba8(width, height)),
            (ColorType::L16, DynamicImage::new_luma16(width, height)),
            (ColorType::La16, DynamicImage::new_luma_a16(width, height)),
            (ColorType::Rgb16, DynamicImage::new_rgb16(width, height)),
            (ColorType::Rgba16, DynamicImage::new_rgba16(width, height)),
        ];

        for (colour, image) in arms {
            let narrowing = Narrowing::of(Format::Png, colour)
                .unwrap_or_else(|| panic!("{colour:?} is a shape the decoders produce"));
            let narrowed = narrow(image).unwrap_or_else(|shape| panic!("{colour:?}: {shape}"));

            assert_eq!(
                narrowed.channels, narrowing.channels,
                "{colour:?}: the header-side and buffer-side channel counts disagree"
            );
            // `peak` adds the page's buffer exactly when `narrow` built one. The moved arms are
            // the two whose colour type is already what the encoder takes.
            let moved = matches!(colour, ColorType::L8 | ColorType::Rgb8);
            assert_eq!(
                narrowing.copies, !moved,
                "{colour:?}: `copies` disagrees with whether the buffer is moved"
            );
            assert_eq!(
                narrowing.peak(u64::from(colour.bytes_per_pixel()) * 2, width, height),
                u128::from(colour.bytes_per_pixel()) * 2
                    + if moved {
                        0
                    } else {
                        narrowed.pixels.len() as u128
                    },
                "{colour:?}: the charged peak is not the two buffers"
            );
        }

        // And the shapes no rule covers are refused by both halves.
        for colour in [ColorType::Rgb32F, ColorType::Rgba32F] {
            assert!(
                Narrowing::of(Format::Png, colour).is_none(),
                "{colour:?} must be refused from the header"
            );
        }
        assert!(narrow(DynamicImage::new_rgb32f(width, height)).is_err());
        assert!(narrow(DynamicImage::new_rgba32f(width, height)).is_err());
    }

    /// A factor table is the shape that invites a guess dressed as a bound, so the table has to
    /// be complete and every row has to carry the version it was measured against.
    ///
    /// Walks the colour types [`Narrowing::of`] admits against the three formats [`read`] is
    /// reached with, and requires a measurement for every pair a decoder can actually produce.
    /// The pairs a decoder cannot produce are asserted the other way: absent from the table,
    /// charged one, and *reported* absent rather than silently indistinguishable from a
    /// measured one.
    ///
    /// [`read`]: super::read
    #[test]
    fn every_arm_the_decoders_produce_is_measured() {
        // What each decoder's `color_type()` can return. png expands its palette and reports
        // one of eight; `image`'s bmp and webp decoders report one of two.
        let produced = [
            (
                Format::Png,
                &[
                    ColorType::L8,
                    ColorType::La8,
                    ColorType::L16,
                    ColorType::La16,
                    ColorType::Rgb8,
                    ColorType::Rgba8,
                    ColorType::Rgb16,
                    ColorType::Rgba16,
                ][..],
            ),
            (Format::Bmp, &[ColorType::Rgb8, ColorType::Rgba8][..]),
            (Format::WebP, &[ColorType::Rgb8, ColorType::Rgba8][..]),
        ];

        let mut measured = 0;
        for (format, colours) in produced {
            for &colour in colours {
                let arm = Scratch::of(format, colour).unwrap_or_else(|| {
                    panic!("{format:?} produces {colour:?} and no factor was measured for it")
                });
                assert!(
                    !arm.against.is_empty(),
                    "{format:?}/{colour:?} names no dependency version"
                );
                assert!(
                    arm.denominator > 0 && arm.numerator >= arm.denominator,
                    "{format:?}/{colour:?}: a decoder cannot allocate less than the buffer it \
                     declares, and a zero denominator is not a factor: {}/{}",
                    arm.numerator,
                    arm.denominator
                );
                measured += 1;
            }
        }
        assert_eq!(
            measured,
            SCRATCH.len(),
            "the table carries a row for an arm no decoder produces, or two rows for one arm"
        );

        // An arm outside the table is charged one, and says it is unmeasured rather than
        // reading as a measured 1.00. `image`'s bmp decoder never reports `L8`.
        let unmeasured =
            Narrowing::of(Format::Bmp, ColorType::L8).expect("`L8` is a shape narrowing covers");
        assert!(
            unmeasured.scratch.is_none(),
            "an arm with no measurement must report that, not carry a plausible number"
        );
        assert_eq!(
            unmeasured.peak(1_000, 10, 10),
            1_000,
            "an unmeasured arm is charged the declared buffer and nothing more"
        );
    }

    /// The charge is the figure that was measured, not the code's own multiplication.
    ///
    /// Every number below is peak resident set from `page::decode` alone on a 6000x8000 page,
    /// less the process base and the entry buffer:
    ///
    /// | arm | declared | measured working set | charged |
    /// |---|---|---|---|
    /// | png `Rgb8` | 144,000,000 | 144.07 MB | 144,000,000 |
    /// | webp `Rgb8`, VP8L | 144,000,000 | 335.83 MB | 336,000,000 |
    /// | webp `Rgb8`, VP8 | 144,000,000 | 224.18 MB | 336,000,000 |
    /// | webp `Rgba8`, VP8 | 192,000,000 | 515.39 MB | 528,000,000 |
    ///
    /// The same page and the same declared buffer, and the charge differs by what one decoder
    /// allocates around it. Asserted at the byte, because a factor held as a rounded decimal
    /// would land near these figures and not on them.
    #[test]
    fn the_charge_is_the_measured_working_set() {
        let (width, height) = (6000, 8000);
        let declared = u64::from(width) * u64::from(height) * 3;
        assert_eq!(declared, 144_000_000);

        let png = Narrowing::of(Format::Png, ColorType::Rgb8).expect("png produces `Rgb8`");
        assert_eq!(png.peak(declared, width, height), 144_000_000);

        let webp = Narrowing::of(Format::WebP, ColorType::Rgb8).expect("webp produces `Rgb8`");
        assert_eq!(webp.peak(declared, width, height), 336_000_000);

        // `Rgba8` covers both codings too, and here VP8 is the worse of the pair: 10.74 bytes a
        // pixel measured against VP8L's 7.00, because VP8 holds its YUV planes and decodes the
        // alpha chunk's own lossless stream while the output buffer is alive. The row charges
        // `2/1`, which with the composited page is eleven bytes a pixel.
        let alpha = Narrowing::of(Format::WebP, ColorType::Rgba8).expect("webp produces `Rgba8`");
        let declared_rgba = u64::from(width) * u64::from(height) * 4;
        assert_eq!(alpha.peak(declared_rgba, width, height), 528_000_000);
        assert!(
            alpha.peak(declared_rgba, width, height) >= 515_385_600,
            "the charge must cover the 10.74 bytes a pixel VP8 with alpha was measured at"
        );

        // Rounded up rather than towards zero, so a bound is never short by a byte.
        assert_eq!(
            webp.peak(1, 1, 1),
            3,
            "1 x 7/3 rounds up to 3, not down to 2"
        );
    }

    /// The factor is per arm, and this is the test that fails if someone folds the worst one
    /// into the per-buffer limit instead.
    ///
    /// One geometry, three arms, one limit: a 6000x8000 webp is refused because it really does
    /// allocate 336 MB, and the same geometry as a png or a bmp is allowed because it really
    /// does not. Lowering the per-buffer limit by webp's factor would refuse all three, which
    /// is the outcome the per-arm table exists to avoid.
    #[test]
    fn widening_one_arm_cannot_refuse_a_page_that_does_not_incur_it() {
        let (width, height) = (6000, 8000);
        let declared = u64::from(width) * u64::from(height) * 3;
        let budget = Budget::default();

        for format in [Format::Png, Format::Bmp] {
            let narrowing = Narrowing::of(format, ColorType::Rgb8).expect("`Rgb8` is produced");
            assert!(
                budget
                    .allow_decoded(narrowing.peak(declared, width, height))
                    .is_ok(),
                "{format:?} allocates the buffer and nothing more, so 144 MB must be allowed"
            );
        }

        let webp = Narrowing::of(Format::WebP, ColorType::Rgb8).expect("`Rgb8` is produced");
        assert!(
            budget
                .allow_decoded(webp.peak(declared, width, height))
                .is_err(),
            "336 MB is over the 256 MiB one buffer may occupy"
        );
    }

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
