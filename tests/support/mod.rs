//! Fixtures the pipeline tests build for themselves.
//!
//! Archives are generated rather than committed. The repository's fixture convention is
//! tiny files — `tests/fixtures/page.jpg` is under two kilobytes — and an archive of pages
//! wide enough to exercise normalisation is two orders of magnitude larger than that.
//!
//! Generating them is not self-referential. The encoder these pages come from is verified
//! against `tests/fixtures/page.jpg`, whose validity was established with a decoder that is
//! not this crate's, and the zip framing is verified independently in
//! `tests/archive_source.rs`.

// Shared by several integration-test binaries, each of which uses a subset: an item unused
// by `archive_source` is exercised by `pipeline`, so `dead_code` here reports the split
// rather than genuinely unreachable code.
#![allow(dead_code)]

use std::fs::{self, File};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use comic_auto_resize::page::{Channels, EncodeSettings, Format, PageImage, encode};
use flate2::write::{DeflateEncoder, ZlibEncoder};
use flate2::{Compression, Crc};
use image::{DynamicImage, GrayImage, ImageBuffer, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// A page of comic-like content: sparse anti-aliased strokes, a gradient, then paper.
///
/// Two properties matter and neither is decorative.
///
/// The strokes are anti-aliased, because a hard black-to-white discontinuity is what a
/// windowed-sinc resampler handles worst. Measured on an earlier version of this generator,
/// 8-pixel hard stripes downscaled by 0.84 gained ringing at every edge and re-encoded to
/// 2.7 times the input's bytes — a property of the pattern, not of the pipeline.
///
/// They are also sparse, because a manga page is mostly paper. A pattern that is a third
/// dense ink costs several times the bytes a real page does without testing anything more.
///
/// Integer arithmetic throughout, so the bytes are identical on every platform.
pub fn page(width: u32, height: u32) -> PageImage {
    /// Stroke period, the ink within it, and the ramp at each ink edge.
    const PERIOD: u32 = 64;
    const INK: u32 = 6;
    const RAMP: u32 = 3;

    let band = height / 3;
    let span = width.saturating_sub(1).max(1);
    let mut pixels = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            let value = if y < band {
                let phase = x % PERIOD;
                if phase < RAMP {
                    u8::try_from(255 - phase * 255 / RAMP).unwrap_or(0)
                } else if phase < RAMP + INK {
                    0
                } else if phase < RAMP + INK + RAMP {
                    u8::try_from((phase - RAMP - INK) * 255 / RAMP).unwrap_or(255)
                } else {
                    255
                }
            } else if y < band * 2 {
                u8::try_from((x * 255 + span / 2) / span).unwrap_or(255)
            } else {
                255
            };
            pixels.extend_from_slice(&[value, value, value]);
        }
    }
    PageImage::new(width, height, Channels::Rgb, pixels).expect("the buffer matches the dimensions")
}

/// One encoded JPEG page.
pub fn page_bytes(width: u32, height: u32) -> Vec<u8> {
    encode(
        "fixture.jpg",
        &page(width, height),
        EncodeSettings::default(),
    )
    .unwrap_or_else(|error| panic!("encoding a {width}x{height} fixture page failed: {error}"))
}

/// The same page as [`page`], encoded as png.
///
/// Generated at test time rather than committed, for the reason the archives are: `image`'s
/// png encoder is under the same feature as its decoder, so this costs no test-only
/// dependency and no blob. The bytes are the encoder's, so a dependency bump that changes
/// them changes both sides at once — which is why the assertions are on geometry and channel
/// count rather than on bytes.
pub fn png_page(width: u32, height: u32) -> Vec<u8> {
    let page = page(width, height);
    encoded(
        &DynamicImage::ImageRgb8(
            RgbImage::from_raw(width, height, page.pixels().to_vec())
                .expect("the buffer matches the dimensions"),
        ),
        ImageFormat::Png,
    )
}

/// The same page encoded as bmp.
pub fn bmp_page(width: u32, height: u32) -> Vec<u8> {
    let page = page(width, height);
    encoded(
        &DynamicImage::ImageRgb8(
            RgbImage::from_raw(width, height, page.pixels().to_vec())
                .expect("the buffer matches the dimensions"),
        ),
        ImageFormat::Bmp,
    )
}

/// The page as an RGB16 png: sixteen bits and no alpha, so the narrowing is observable on
/// its own rather than alongside a composite.
pub fn png_rgb16_page(width: u32, height: u32) -> Vec<u8> {
    let page = page(width, height);
    // `v * 257` is the exact 8-to-16-bit widening, so every sample's high byte is the
    // generator's own value and the narrowing is checkable against it.
    let samples: Vec<u16> = page
        .pixels()
        .iter()
        .map(|&sample| u16::from(sample) * 257)
        .collect();
    encoded(
        &DynamicImage::ImageRgb16(
            ImageBuffer::<Rgb<u16>, Vec<u16>>::from_raw(width, height, samples)
                .expect("the buffer matches the dimensions"),
        ),
        ImageFormat::Png,
    )
}

/// The page as a png that *declares* itself an animation.
///
/// An `acTL` chunk spliced in before the first `IDAT`, which is what `PngDecoder::is_apng`
/// reports. The declaration is the case under test: a container that says "animation" has to
/// be refused rather than have its first frame taken as the page, so the `fdAT` frames a
/// complete APNG would carry are neither needed nor written.
pub fn apng_page(width: u32, height: u32) -> Vec<u8> {
    let png = png_page(width, height);
    let at = png_chunk_offset(&png, *b"IDAT").expect("an encoded png has an IDAT chunk");
    let mut spliced = png[..at].to_vec();
    // Two frames, looping forever.
    spliced.extend_from_slice(&png_chunk(*b"acTL", &[0, 0, 0, 2, 0, 0, 0, 0]));
    spliced.extend_from_slice(&png[at..]);
    spliced
}

/// A webp container that declares itself an animation.
///
/// Hand-assembled, because `image`'s webp encoder writes single-frame VP8L only. What
/// `image_webp` requires before it reports an animation: a `VP8X` header with the animation
/// flag set, an `ANIM` chunk, and an `ANMF` chunk of at least 24 bytes. The frame's pixels
/// are not among them — the refusal comes before any frame is decoded, which is the point.
pub fn animated_webp(width: u32, height: u32) -> Vec<u8> {
    let mut body = b"WEBP".to_vec();

    // Flags: bit 1 is the animation flag. Then three reserved bytes and the canvas, each
    // axis stored one less than its real value.
    let mut vp8x = vec![0b0000_0010, 0, 0, 0];
    vp8x.extend_from_slice(&three(width - 1));
    vp8x.extend_from_slice(&three(height - 1));
    body.extend_from_slice(&riff_chunk(*b"VP8X", &vp8x));

    // A white background hint, then a loop count of zero: loop forever.
    body.extend_from_slice(&riff_chunk(*b"ANIM", &[0xFF, 0xFF, 0xFF, 0xFF, 0, 0]));

    // One frame header — offsets, dimensions, duration, flags — padded to the 24 bytes the
    // reader requires before it will count the frame.
    let mut anmf = Vec::new();
    anmf.extend_from_slice(&three(0));
    anmf.extend_from_slice(&three(0));
    anmf.extend_from_slice(&three(width - 1));
    anmf.extend_from_slice(&three(height - 1));
    anmf.extend_from_slice(&three(100));
    anmf.resize(24, 0);
    body.extend_from_slice(&riff_chunk(*b"ANMF", &anmf));

    let mut webp = b"RIFF".to_vec();
    webp.extend_from_slice(&len32(&body).to_le_bytes());
    webp.extend_from_slice(&body);
    webp
}

/// `bytes` cut down to the part a header read needs, so the geometry is still readable and the
/// pixels are not there at all.
///
/// This is what makes the budget's ordering falsifiable. Against an internally consistent
/// fixture, an implementation that decoded first and checked afterwards would refuse the page
/// too, and for the same stated reason — so the test would pass on the wrong code. Against
/// these, a decode-first implementation reports a decode failure and only a header-first one
/// reports the budget.
///
/// Per format, the cut is at the last byte a decoder needs to report `dimensions()`:
///
/// - **png**: through the first `IDAT`'s length and type plus four bytes of its data.
///   `Decoder::read_info` stops at `IDAT` — it does not inflate it — so the geometry comes off
///   `IHDR` and inflating the stub fails.
/// - **bmp**: the 14-byte `BITMAPFILEHEADER` and the 40-byte `BITMAPINFOHEADER`. A 24-bit bmp
///   carries no palette, so `read_metadata` needs nothing after them.
/// - **webp**: the `RIFF`/`WEBP` header, the `VP8L` chunk header, and the five payload bytes
///   that carry the signature and the 14-bit dimensions.
pub fn header_only(bytes: &[u8], format: Format) -> Vec<u8> {
    let at = match format {
        Format::Png => png_chunk_offset(bytes, *b"IDAT").expect("an encoded png has an IDAT") + 12,
        Format::Bmp => 54,
        Format::WebP => 25,
        Format::Jpeg => panic!("the JPEG path has its own forged-header fixture"),
    };
    bytes[..at].to_vec()
}

/// A png whose `iCCP` chunk inflates to `inflated` bytes from a payload of a few hundred.
///
/// The trigger an independent review found: `png`'s `read_info` parses every chunk before the
/// first `IDAT`, and `parse_iccp_raw` inflates the profile bounded only by the decoder's own
/// `Limits`. Under `PngDecoder::new` — which is `no_limits` — that bound is `usize::MAX`, so
/// this fixture allocates `inflated` bytes during *header* parsing, before the dimensions are
/// readable and therefore before any budget could see them. `Vec` growth ends in
/// `handle_alloc_error`, which aborts rather than unwinds, so the pipeline's `catch_unwind`
/// cannot intercept it.
///
/// The image itself is 1x1, so nothing about its declared size is remarkable; the whole of the
/// attack is in the ancillary chunk. Deflate's ceiling is 1032:1, so `inflated` bytes cost
/// about `inflated / 1000` in the fixture.
pub fn png_with_inflating_profile(inflated: usize) -> Vec<u8> {
    let png = png_page(1, 1);
    let idat = png_chunk_offset(&png, *b"IDAT").expect("an encoded png has an IDAT");

    // A zlib stream — `parse_iccp_raw` expects one — of a run of identical bytes, which is
    // what compresses at close to Deflate's ceiling.
    let mut zlib = ZlibEncoder::new(Vec::new(), Compression::best());
    zlib.write_all(&vec![0u8; inflated])
        .expect("writing to a Vec cannot fail");
    let compressed = zlib.finish().expect("flushing to a Vec cannot fail");

    // `iCCP` is a NUL-terminated profile name, one compression-method byte, then the stream.
    let mut iccp = b"c\0\0".to_vec();
    iccp.extend_from_slice(&compressed);

    let mut spliced = png[..idat].to_vec();
    spliced.extend_from_slice(&png_chunk(*b"iCCP", &iccp));
    spliced.extend_from_slice(&png[idat..]);
    spliced
}

/// A png carrying a `tEXt` chunk of `comment` bytes before its first `IDAT`.
///
/// Legal, and the case a first draft of the decoder's memory pool refused: `png`'s `Limits` is a
/// decrementing pool that charges a chunk *twice* — once for the capacity growth of the buffer
/// holding it, which doubles and so costs up to `2L`, and again for the chunk's own length when
/// it is `tEXt`, `zTXt` or `iTXt`. A pool sized at one entry plus a scanline therefore refused a
/// 3.2 MB page over a 2 MiB comment, and the run exited non-zero with no archive.
pub fn png_with_text_chunk(width: u32, height: u32, comment: usize) -> Vec<u8> {
    let png = png_page(width, height);
    let idat = png_chunk_offset(&png, *b"IDAT").expect("an encoded png has an IDAT");

    // A NUL-separated keyword and text, which is what `tEXt` holds.
    let mut text = b"Comment\0".to_vec();
    text.resize(text.len() + comment, b'x');

    let mut spliced = png[..idat].to_vec();
    spliced.extend_from_slice(&png_chunk(*b"tEXt", &text));
    spliced.extend_from_slice(&png[idat..]);
    spliced
}

/// The offset of the first `kind` chunk's length field, walking chunk lengths rather than
/// searching for the type: a chunk's *data* may hold the four bytes being looked for.
fn png_chunk_offset(png: &[u8], kind: [u8; 4]) -> Option<usize> {
    let mut at = 8;
    while at + 8 <= png.len() {
        if png[at + 4..at + 8] == kind {
            return Some(at);
        }
        let length = u32::from_be_bytes(png[at..at + 4].try_into().ok()?);
        at += 12 + usize::try_from(length).ok()?;
    }
    None
}

/// A png chunk: length, type, data, then a CRC over the type and the data.
fn png_chunk(kind: [u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = len32(data).to_be_bytes().to_vec();
    chunk.extend_from_slice(&kind);
    chunk.extend_from_slice(data);
    let mut checked = kind.to_vec();
    checked.extend_from_slice(data);
    chunk.extend_from_slice(&crc32(&checked).to_be_bytes());
    chunk
}

/// A RIFF chunk: four-byte type, little-endian size, payload padded to an even length.
fn riff_chunk(kind: [u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = kind.to_vec();
    chunk.extend_from_slice(&len32(data).to_le_bytes());
    chunk.extend_from_slice(data);
    if data.len() % 2 == 1 {
        chunk.push(0);
    }
    chunk
}

/// A 24-bit little-endian value, as webp's headers store a dimension.
fn three(value: u32) -> [u8; 3] {
    let bytes = value.to_le_bytes();
    [bytes[0], bytes[1], bytes[2]]
}

/// The same page encoded as a lossless webp.
///
/// `image`'s webp encoder is VP8L-only, which is all this needs: the fixture has to be a
/// webp container the decoder reads, not a demonstration of VP8 rate control.
pub fn webp_page(width: u32, height: u32) -> Vec<u8> {
    let page = page(width, height);
    encoded(
        &DynamicImage::ImageRgb8(
            RgbImage::from_raw(width, height, page.pixels().to_vec())
                .expect("the buffer matches the dimensions"),
        ),
        ImageFormat::WebP,
    )
}

/// The same page as a grayscale png, so the one-component path is exercised through a
/// decoder that is not libjpeg.
pub fn png_gray_page(width: u32, height: u32) -> Vec<u8> {
    let page = page(width, height);
    let grey: Vec<u8> = page.pixels().iter().step_by(3).copied().collect();
    encoded(
        &DynamicImage::ImageLuma8(
            GrayImage::from_raw(width, height, grey).expect("one channel of three"),
        ),
        ImageFormat::Png,
    )
}

/// The page as an RGBA8 png whose alpha channel is `alpha` everywhere.
///
/// `0xFF` is the case the composite must leave alone — the one that would break silently —
/// and anything lower is a page whose appearance the rule changes.
pub fn png_rgba_page(width: u32, height: u32, alpha: u8) -> Vec<u8> {
    encoded(
        &DynamicImage::ImageRgba8(with_alpha(width, height, alpha)),
        ImageFormat::Png,
    )
}

/// The page as an RGBA8 png whose left half is transparent and right half opaque.
///
/// One fixture rather than two, because the property is that the two halves come out
/// *differently*: the transparent side becomes paper and the opaque side is untouched.
pub fn png_half_transparent_page(width: u32, height: u32) -> Vec<u8> {
    let mut image = with_alpha(width, height, u8::MAX);
    for (x, _, pixel) in image.enumerate_pixels_mut() {
        if x < width / 2 {
            pixel.0[3] = 0;
        }
    }
    encoded(&DynamicImage::ImageRgba8(image), ImageFormat::Png)
}

/// The page as an RGBA16 png, so both channels the encoder cannot carry arrive at once.
pub fn png_rgba16_page(width: u32, height: u32, alpha: u16) -> Vec<u8> {
    let page = page(width, height);
    let mut samples = Vec::with_capacity((width * height * 4) as usize);
    for pixel in page.pixels().chunks_exact(3) {
        // `v * 257` is the exact 8-to-16-bit widening, so the fixture's high bytes are the
        // generator's own values and the narrowing is checkable against them.
        samples.extend(pixel.iter().map(|&sample| u16::from(sample) * 257));
        samples.push(alpha);
    }
    encoded(
        &DynamicImage::ImageRgba16(
            ImageBuffer::<Rgba<u16>, Vec<u16>>::from_raw(width, height, samples)
                .expect("four channels of three"),
        ),
        ImageFormat::Png,
    )
}

/// The page with a uniform alpha channel appended.
fn with_alpha(width: u32, height: u32, alpha: u8) -> RgbaImage {
    let page = page(width, height);
    let mut samples = Vec::with_capacity((width * height * 4) as usize);
    for pixel in page.pixels().chunks_exact(3) {
        samples.extend_from_slice(pixel);
        samples.push(alpha);
    }
    RgbaImage::from_raw(width, height, samples).expect("four channels of three")
}

/// `image` encoding `image` into `format`, in memory.
fn encoded(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, format)
        .unwrap_or_else(|error| panic!("encoding a {format:?} fixture failed: {error}"));
    bytes.into_inner()
}

/// The BMP conformance corpus, or `None` with the script that writes it.
///
/// The convention the rar and 7z suites already follow: a test that silently does not run is
/// worse than one that says why. The corpus arrives by fetch rather than by commit because
/// its generator is GPL-3.0 and is run rather than conveyed.
pub fn bmp_fixtures() -> Result<PathBuf, String> {
    let root = std::env::var_os("CAR_BMP_FIXTURES").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools/bmp-fixtures"),
        PathBuf::from,
    );
    if root.join("g").is_dir() {
        return Ok(root);
    }
    Err(format!(
        "the BMP conformance corpus is not at {}; run tests/fixtures/make-bmp-fixtures.sh, \
         or set CAR_BMP_FIXTURES",
        root.display()
    ))
}

/// Writes a Stored zip holding `entries` in exactly the order given.
///
/// `large_file(false)` so no Zip64 extra field appears and the fixture stays a plain 32-bit
/// archive, which is what the hand-written framings in `framed_archive` are written against.
pub fn write_archive(path: &Path, entries: &[(String, Vec<u8>)]) {
    let file = File::create(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let mut writer = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .large_file(false);
    for (name, bytes) in entries {
        writer
            .start_file(name.as_str(), options)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        writer
            .write_all(bytes)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
    }
    writer
        .finish()
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
}

/// Writes an archive of `count` identical pages, named `pages/pageNNNN.jpg`.
///
/// Identical pages on purpose: it is what makes a 100-page run and a 1000-page run
/// comparable at the same page size.
pub fn write_pages(path: &Path, count: u32, width: u32, height: u32) {
    let bytes = page_bytes(width, height);
    let entries: Vec<_> = (0..count)
        .map(|index| (format!("pages/page{:04}.jpg", index + 1), bytes.clone()))
        .collect();
    write_archive(path, &entries);
}

/// How a hand-written zip departs from what `ZipWriter` produces.
///
/// `ZipWriter::new_stream` can write the data-descriptor form, but nothing can write an
/// archive whose directory order and layout disagree, whose entries record a size they do not
/// hold, whose directory is cut short, or whose record points at no local header. One
/// generator for all five, assembled field by field, rather than two sources of fixture.
#[derive(Clone, Copy, Debug, Default)]
pub struct Framing {
    /// Each local header records zero sizes and sets general-purpose flag bit 3; the real
    /// sizes follow the entry's data in a trailing descriptor.
    pub data_descriptors: bool,
    /// Entry data is laid out back to front, so the central directory's order and the
    /// local-header sequence disagree.
    pub data_reversed: bool,
    /// The size every entry records, in place of its real one. The data is written as it is,
    /// so an archive declaring more than it holds is shorter than its own claim — which is
    /// what makes "refused without being read" observable.
    pub declared_size: Option<u32>,
    /// Bytes cut from the end of the central directory, while the end-of-central-directory
    /// record still describes its full length.
    pub truncated_directory: usize,
    /// The entry whose central-directory record points at an offset holding no local
    /// header, so the table reads but that one entry cannot be located.
    pub orphaned_entry: Option<usize>,
    /// The *total* entry count the end record states, in place of the real one, leaving the
    /// count of entries on this disk truthful. The two fields are equal in every conformant
    /// single-disk archive, and a reader must count with the one `zip` counts with.
    pub recorded_total: Option<u16>,
    /// Bytes appended after the end record. The format allows only the record's own comment
    /// there, but readers tolerate garbage, so a reader must too.
    pub trailing_bytes: usize,
    /// The length of the archive comment the end record states and carries. The format allows
    /// up to 65,535 bytes there, which pushes the record that far from the end of the file.
    pub comment_bytes: usize,
    /// The entry data is Deflate-compressed rather than Stored. Together with a
    /// `declared_size` smaller than the data, this is the shape a decompression bomb takes
    /// once the recorded size is checked before the read: the record is modest and the stream
    /// is not.
    pub deflated: bool,
}

/// Writes a zip byte by byte, with `framing`'s departures from the ordinary form.
pub fn framed_archive(entries: &[(&str, Vec<u8>)], framing: Framing) -> Vec<u8> {
    const LOCAL_HEADER: u32 = 0x0403_4b50;
    const DATA_DESCRIPTOR: u32 = 0x0807_4b50;
    const CENTRAL_HEADER: u32 = 0x0201_4b50;
    const END_OF_DIRECTORY: u32 = 0x0605_4b50;
    /// Version 2.0, the floor for Stored with a data descriptor.
    const VERSION: u16 = 20;
    const STORED: u16 = 0;
    const DEFLATED: u16 = 8;
    /// General-purpose bit 3: the sizes are in a trailing descriptor, not this header.
    const SIZES_IN_DESCRIPTOR: u16 = 1 << 3;

    let flag = if framing.data_descriptors {
        SIZES_IN_DESCRIPTOR
    } else {
        0
    };
    let mut bytes = Vec::new();
    let mut offsets = vec![0; entries.len()];

    let mut layout: Vec<usize> = (0..entries.len()).collect();
    if framing.data_reversed {
        layout.reverse();
    }
    let method = if framing.deflated { DEFLATED } else { STORED };
    for index in layout {
        let (name, data) = &entries[index];
        let payload = if framing.deflated {
            deflate(data)
        } else {
            data.clone()
        };
        // The recorded sizes: the compressed one is real, because the entry has to be
        // readable, and the uncompressed one is whatever the fixture wants recorded.
        let compressed = len32(&payload);
        let uncompressed = framing.declared_size.unwrap_or_else(|| len32(data));
        offsets[index] = len32(&bytes);
        // Zeroed here and repeated after the data when the descriptor form is asked for,
        // which is the whole of what makes such an archive unreadable from local headers.
        let (header_crc, header_compressed, header_uncompressed) = if framing.data_descriptors {
            (0, 0, 0)
        } else {
            (crc32(data), compressed, uncompressed)
        };

        push32(&mut bytes, LOCAL_HEADER);
        push16(&mut bytes, VERSION);
        push16(&mut bytes, flag);
        push16(&mut bytes, method);
        push32(&mut bytes, 0); // modification time and date
        push32(&mut bytes, header_crc);
        push32(&mut bytes, header_compressed);
        push32(&mut bytes, header_uncompressed);
        push16(&mut bytes, len16(name));
        push16(&mut bytes, 0); // extra field
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&payload);

        if framing.data_descriptors {
            push32(&mut bytes, DATA_DESCRIPTOR);
            push32(&mut bytes, crc32(data));
            push32(&mut bytes, compressed);
            push32(&mut bytes, uncompressed);
        }
    }

    if let Some(orphan) = framing.orphaned_entry {
        // The last entry's final data byte, which is past every entry's start, so no entry's
        // data region overruns the orphan's own. The four bytes read there are that byte
        // followed by the central directory's signature, which is not a local header's.
        offsets[orphan] = len32(&bytes).saturating_sub(1);
    }

    let directory_offset = len32(&bytes);
    let mut directory = Vec::new();
    for (index, (name, data)) in entries.iter().enumerate() {
        let compressed = if framing.deflated {
            len32(&deflate(data))
        } else {
            len32(data)
        };
        let uncompressed = framing.declared_size.unwrap_or_else(|| len32(data));
        push32(&mut directory, CENTRAL_HEADER);
        push16(&mut directory, VERSION); // version made by
        push16(&mut directory, VERSION); // version needed
        push16(&mut directory, flag);
        push16(&mut directory, method);
        push32(&mut directory, 0); // modification time and date
        push32(&mut directory, crc32(data));
        push32(&mut directory, compressed);
        push32(&mut directory, uncompressed);
        push16(&mut directory, len16(name));
        push16(&mut directory, 0); // extra field
        push16(&mut directory, 0); // comment
        push16(&mut directory, 0); // starting disk
        push16(&mut directory, 0); // internal attributes
        push32(&mut directory, 0); // external attributes
        push32(&mut directory, offsets[index]);
        directory.extend_from_slice(name.as_bytes());
    }

    // Recorded before truncation, so the end record describes a directory longer than the
    // one that is there.
    let directory_len = len32(&directory);
    directory.truncate(directory.len().saturating_sub(framing.truncated_directory));
    bytes.extend_from_slice(&directory);

    let count = u16::try_from(entries.len()).expect("the fixture holds few entries");
    push32(&mut bytes, END_OF_DIRECTORY);
    push16(&mut bytes, 0); // this disk
    push16(&mut bytes, 0); // the disk the directory starts on
    push16(&mut bytes, count); // entries on this disk
    push16(&mut bytes, framing.recorded_total.unwrap_or(count)); // entries in total
    push32(&mut bytes, directory_len);
    push32(&mut bytes, directory_offset);
    let comment = u16::try_from(framing.comment_bytes).expect("a comment is at most 65,535 bytes");
    push16(&mut bytes, comment); // archive comment length
    bytes.resize(bytes.len() + framing.comment_bytes, b'#');
    bytes.resize(bytes.len() + framing.trailing_bytes, 0);
    bytes
}

/// How an entry of an [`encoded_archive`] is encrypted, as its *headers* declare it.
///
/// The data is not actually enciphered, and it does not need to be: every path these fixtures
/// exercise refuses the entry before reading a byte of it. A fixture whose data must really
/// decrypt is written by `ZipWriter` with `with_deprecated_encryption` instead, because
/// producing a `ZipCrypto` keystream by hand would be reimplementing the cipher to test the
/// reader that uses it.
#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub enum Encryption {
    #[default]
    None,
    /// General-purpose bit 0, which is all a `ZipCrypto` entry declares.
    ZipCrypto,
    /// Bit 0, compression method 99, and an AE-x extra field naming `AES-256`. All three,
    /// because `zip` refuses "AES encryption without AES extra data field" when the method
    /// says 99 and the field is absent — and because `AexEncryption::parse` then rewrites the
    /// compression method to the *underlying* one, which is what makes an AES entry
    /// indistinguishable from a `ZipCrypto` one by the open error alone.
    Aes256,
}

/// One entry of a zip named by the bytes the archive stores, rather than by a `&str`.
///
/// `ZipWriter` cannot write these at all: it takes a name as `&str`, encodes it as UTF-8, and
/// sets general-purpose bit 11 for anything non-ASCII — which is the exact opposite of the
/// archive under test, whose names are in a legacy codepage with that bit clear.
pub struct Encoded<'a> {
    /// The name exactly as it will appear in both headers.
    pub name: &'a [u8],
    pub data: Vec<u8>,
    /// General-purpose bit 11: the archive declaring this name to be UTF-8.
    pub utf8: bool,
    /// An Info-ZIP Unicode Path extra field (`0x7075`) carrying a name of its own.
    ///
    /// Its `NameCRC32` is computed over `name`, so the field reads as current even when its
    /// characters disagree with what `name` decodes to — which is the case worth testing,
    /// since a stale CRC makes `zip` refuse the whole archive.
    pub unicode_path: Option<&'a str>,
    pub encryption: Encryption,
    /// The compression method the headers declare, in place of Stored.
    ///
    /// The data is written uncompressed whatever this says, and that is the point: the methods
    /// worth naming here are the ones this build cannot decode, so the entry is refused before
    /// anything tries.
    pub method: Option<u16>,
}

impl<'a> Encoded<'a> {
    /// A plain stored entry, bit 11 clear: the legacy form a Japanese archiver writes.
    pub fn new(name: &'a [u8], data: Vec<u8>) -> Self {
        Self {
            name,
            data,
            utf8: false,
            unicode_path: None,
            encryption: Encryption::None,
            method: None,
        }
    }

    /// The same entry with the archive declaring its name to be UTF-8.
    pub fn utf8(mut self) -> Self {
        self.utf8 = true;
        self
    }

    pub fn unicode_path(mut self, name: &'a str) -> Self {
        self.unicode_path = Some(name);
        self
    }

    pub fn encrypted(mut self, encryption: Encryption) -> Self {
        self.encryption = encryption;
        self
    }

    /// The same entry declaring a compression method in place of Stored.
    pub fn compressed_as(mut self, method: u16) -> Self {
        self.method = Some(method);
        self
    }

    fn flag(&self) -> u16 {
        /// General-purpose bit 0: the data is enciphered.
        const ENCRYPTED: u16 = 1;
        /// General-purpose bit 11: the name and comment are UTF-8.
        const UTF8_NAME: u16 = 1 << 11;

        let mut flag = 0;
        if self.utf8 {
            flag |= UTF8_NAME;
        }
        if self.encryption != Encryption::None {
            flag |= ENCRYPTED;
        }
        flag
    }

    fn method(&self) -> u16 {
        if self.encryption == Encryption::Aes256 {
            return 99;
        }
        self.method.unwrap_or(0)
    }

    /// The central record's extra field, which is the one `zip` parses for an entry.
    fn extra(&self) -> Vec<u8> {
        let mut extra = Vec::new();
        if let Some(unicode) = self.unicode_path {
            push16(&mut extra, 0x7075);
            push16(&mut extra, len16_of(5 + unicode.len()));
            extra.push(1); // version
            push32(&mut extra, crc32(self.name));
            extra.extend_from_slice(unicode.as_bytes());
        }
        if self.encryption == Encryption::Aes256 {
            push16(&mut extra, 0x9901);
            push16(&mut extra, 7);
            push16(&mut extra, 2); // AE-2
            extra.extend_from_slice(b"AE"); // vendor id
            extra.push(3); // AES-256
            push16(&mut extra, 0); // the underlying method, which parsing restores
        }
        extra
    }
}

/// Writes a zip byte by byte whose entry names are the bytes given.
///
/// Stored, no data descriptors, no Zip64: the departures from the ordinary form that
/// [`framed_archive`] exists for are orthogonal to this one, and combining them would make one
/// generator that answers neither question clearly.
///
/// The extra fields go in the central record only, which is where `zip` reads an entry's:
/// `central_header_to_zip_file_inner` parses that record's field and never the local one. The
/// local header therefore declares no extra field, which keeps it self-consistent for
/// `find_data_start`.
pub fn encoded_archive(entries: &[Encoded<'_>]) -> Vec<u8> {
    const LOCAL_HEADER: u32 = 0x0403_4b50;
    const CENTRAL_HEADER: u32 = 0x0201_4b50;
    const END_OF_DIRECTORY: u32 = 0x0605_4b50;
    /// Version 2.0, the floor for Stored.
    const VERSION: u16 = 20;

    let mut bytes = Vec::new();
    let mut offsets = Vec::with_capacity(entries.len());
    for entry in entries {
        offsets.push(len32(&bytes));
        push32(&mut bytes, LOCAL_HEADER);
        push16(&mut bytes, VERSION);
        push16(&mut bytes, entry.flag());
        push16(&mut bytes, entry.method());
        push32(&mut bytes, 0); // modification time and date
        push32(&mut bytes, crc32(&entry.data));
        push32(&mut bytes, len32(&entry.data));
        push32(&mut bytes, len32(&entry.data));
        push16(&mut bytes, len16_of(entry.name.len()));
        push16(&mut bytes, 0); // extra field
        bytes.extend_from_slice(entry.name);
        bytes.extend_from_slice(&entry.data);
    }

    let directory_offset = len32(&bytes);
    let mut directory = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let extra = entry.extra();
        push32(&mut directory, CENTRAL_HEADER);
        push16(&mut directory, VERSION); // version made by
        push16(&mut directory, VERSION); // version needed
        push16(&mut directory, entry.flag());
        push16(&mut directory, entry.method());
        push32(&mut directory, 0); // modification time and date
        push32(&mut directory, crc32(&entry.data));
        push32(&mut directory, len32(&entry.data));
        push32(&mut directory, len32(&entry.data));
        push16(&mut directory, len16_of(entry.name.len()));
        push16(&mut directory, len16_of(extra.len()));
        push16(&mut directory, 0); // comment
        push16(&mut directory, 0); // starting disk
        push16(&mut directory, 0); // internal attributes
        push32(&mut directory, 0); // external attributes
        push32(&mut directory, offsets[index]);
        directory.extend_from_slice(entry.name);
        directory.extend_from_slice(&extra);
    }

    let directory_len = len32(&directory);
    bytes.extend_from_slice(&directory);

    let count = u16::try_from(entries.len()).expect("the fixture holds few entries");
    push32(&mut bytes, END_OF_DIRECTORY);
    push16(&mut bytes, 0); // this disk
    push16(&mut bytes, 0); // the disk the directory starts on
    push16(&mut bytes, count); // entries on this disk
    push16(&mut bytes, count); // entries in total
    push32(&mut bytes, directory_len);
    push32(&mut bytes, directory_offset);
    push16(&mut bytes, 0); // archive comment length
    bytes
}

fn len16_of(length: usize) -> u16 {
    u16::try_from(length).expect("a fixture name and extra field are short")
}

fn push16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn len32(bytes: &[u8]) -> u32 {
    u32::try_from(bytes.len()).expect("a fixture archive stays well under 4 GiB")
}

fn len16(name: &str) -> u16 {
    u16::try_from(name.len()).expect("a fixture entry name is short")
}

/// The checksum every zip entry records, from the crate `zip` itself uses.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = Crc::new();
    crc.update(data);
    crc.sum()
}

/// Raw Deflate, which is what a zip entry carries — no zlib wrapper.
fn deflate(data: &[u8]) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(data).expect("deflate accepts any input");
    encoder.finish().expect("deflate finishes")
}

/// A directory removed when it goes out of scope.
///
/// Hand-rolled rather than a `tempfile` dependency: every dependency, development ones
/// included, goes through `cargo deny`, and this is twenty lines.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "comic-auto-resize-{label}-{}-{unique}",
            std::process::id()
        ));
        // A leftover from a previous run would make the output-refusal tests lie.
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The 7-Zip command-line archiver, or `None` with a message naming what to install.
///
/// Unlike rar, 7z has an open writer, so the fixtures need no committed blob and no manual
/// step — but a machine without `7zz` should say so rather than fail, the way the rar tests
/// already do. A test that silently does not run is worse than one that says why.
pub fn seven_zip() -> Option<&'static str> {
    for program in ["7zz", "7z"] {
        if std::process::Command::new(program)
            .arg("i")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            return Some(program);
        }
    }
    eprintln!(
        "SKIP: no 7-Zip command-line archiver on PATH. Install one (`brew install sevenzip`, \
         `choco install 7zip`, or your distribution's `p7zip`) to run the 7z tests."
    );
    None
}

/// Writes `files` into a 7z at `archive`, staging them under `staging` first.
///
/// `flags` go to `7zz a` before the archive name, which is where the shape of the fixture is
/// chosen: `-m0=LZMA2:d…` for a dictionary size, `-ms=off` for one block per entry, `-spf`
/// for a name the archiver would otherwise strip, `-p…` for encryption.
///
/// The entry order is 7-Zip's, not this call's: it sorts, so a fixture that needs a stored
/// order asserts the order `7zz l` reports rather than the order the names were given in.
pub fn write_seven_zip(archive: &Path, staging: &Path, files: &[(&str, Vec<u8>)], flags: &[&str]) {
    let program = seven_zip().expect("the caller checked for 7-Zip");
    write_tree(staging, files);

    // One argument per top-level name, so a subdirectory is added whole and its entries keep
    // their path prefix.
    let mut tops: Vec<&str> = files
        .iter()
        .map(|(name, _)| name.split('/').next().unwrap_or(name))
        .collect();
    tops.sort_unstable();
    tops.dedup();

    let output = std::process::Command::new(program)
        .args(["a", "-t7z", "-bso0", "-bsp0"])
        .args(flags)
        .arg(archive)
        .args(&tops)
        .current_dir(staging)
        .output()
        .expect("runs the 7-Zip archiver");
    assert!(
        output.status.success(),
        "{program} a {flags:?} {} {tops:?} failed: {}{}",
        archive.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Every entry name a 7z holds, in stored order, according to an implementation that is not
/// the one under test.
///
/// Change 1's lesson: a fixture validated only by the reader it was written for cannot
/// attribute a failure. `7zz l -slt` reports the header's own order.
pub fn seven_zip_listing(archive: &Path) -> Vec<String> {
    let program = seven_zip().expect("the caller checked for 7-Zip");
    let output = std::process::Command::new(program)
        .args(["l", "-ba", "-slt"])
        .arg(archive)
        .output()
        .expect("runs the 7-Zip archiver");
    assert!(output.status.success(), "{program} l failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("Path = "))
        .map(str::to_owned)
        .collect()
}

/// Writes `files` as a directory tree under `root`, creating the directories each needs.
///
/// A name may carry `/` separators; everything before the last one becomes a directory.
pub fn write_tree(root: &Path, files: &[(&str, Vec<u8>)]) {
    fs::create_dir_all(root).unwrap_or_else(|error| panic!("{}: {error}", root.display()));
    for (name, bytes) in files {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("{}: {error}", parent.display()));
        }
        fs::write(&path, bytes).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    }
}

/// The dimensions a JPEG's start-of-frame header declares.
///
/// Walked by segment length rather than scanned for `FF C0`: a quantisation table entry of
/// `FF` followed by an arbitrary quantiser byte occurs inside `DQT`.
pub fn jpeg_size(jpeg: &[u8]) -> Option<(u32, u32)> {
    let mut index = 2;
    while index + 9 < jpeg.len() {
        if jpeg[index] != 0xFF {
            return None;
        }
        match jpeg[index + 1] {
            0xFF => index += 1,
            0xC0..=0xC2 => {
                let height = u16::from_be_bytes([jpeg[index + 5], jpeg[index + 6]]);
                let width = u16::from_be_bytes([jpeg[index + 7], jpeg[index + 8]]);
                return Some((u32::from(width), u32::from(height)));
            }
            0xDA | 0xD9 => return None,
            0x01 | 0xD0..=0xD8 => index += 2,
            _ => {
                let length = usize::from(u16::from_be_bytes([jpeg[index + 2], jpeg[index + 3]]));
                index += 2 + length;
            }
        }
    }
    None
}

/// The start-of-frame marker byte, which says whether a file is baseline (`C0`), extended
/// sequential (`C1`), or progressive (`C2`).
///
/// Walked the same way as [`jpeg_size`], and for the same reason: `FF` followed by an
/// arbitrary quantiser byte occurs inside `DQT`, so a byte scan can find a frame header that
/// is not one.
pub fn start_of_frame(jpeg: &[u8]) -> Option<u8> {
    let mut index = 2;
    while index + 9 < jpeg.len() {
        if jpeg[index] != 0xFF {
            return None;
        }
        match jpeg[index + 1] {
            0xFF => index += 1,
            marker @ 0xC0..=0xC2 => return Some(marker),
            0xDA | 0xD9 => return None,
            0x01 | 0xD0..=0xD8 => index += 2,
            _ => {
                let length = usize::from(u16::from_be_bytes([jpeg[index + 2], jpeg[index + 3]]));
                index += 2 + length;
            }
        }
    }
    None
}

/// Flips every bit of one byte of entropy-coded data, `offset` bytes past the scan header.
///
/// The scan is located by walking segment lengths, not by searching for `FF DA`: a
/// quantisation table entry of `FF` followed by an arbitrary quantiser byte occurs inside
/// `DQT`, so a byte search can land in the wrong place and corrupt nothing.
pub fn corrupt_scan(jpeg: &[u8], offset: usize) -> Vec<u8> {
    let mut index = 2;
    let scan = loop {
        assert!(index + 4 < jpeg.len(), "no start-of-scan found");
        assert_eq!(jpeg[index], 0xFF, "expected a marker at {index}");
        match jpeg[index + 1] {
            0xFF => index += 1,
            0xDA => break index,
            0x01 | 0xD0..=0xD8 => index += 2,
            _ => {
                let length = usize::from(u16::from_be_bytes([jpeg[index + 2], jpeg[index + 3]]));
                index += 2 + length;
            }
        }
    };

    let header_len = usize::from(u16::from_be_bytes([jpeg[scan + 2], jpeg[scan + 3]]));
    let target = scan + 2 + header_len + offset;
    assert!(
        target < jpeg.len(),
        "offset {offset} is past the end of the scan"
    );

    let mut damaged = jpeg.to_vec();
    damaged[target] ^= 0xFF;
    damaged
}

/// The options `--fix-idx` produces, which is the only naming a test ever asks for beside the
/// default.
pub fn by_position() -> comic_auto_resize::source::ReadOptions {
    comic_auto_resize::source::ReadOptions {
        naming: comic_auto_resize::source::Naming::ByPosition,
        ..Default::default()
    }
}

/// Every entry of a zip, in stored order, as `(name, bytes)`.
pub fn read_archive(path: &Path) -> Vec<(String, Vec<u8>)> {
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let mut source = comic_auto_resize::source::ZipSource::new(
        std::io::Cursor::new(bytes),
        &comic_auto_resize::source::ReadOptions::default(),
    )
    .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let mut entries = Vec::new();
    while let Some(entry) = source.next_entry() {
        let entry = entry.unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        entries.push((entry.name, entry.bytes));
    }
    entries
}

/// Writes a zip whose entries are really ZipCrypto-enciphered, under `password`.
///
/// `zip`'s own writer, because the cipher has to be the one the reader will run in reverse.
/// `mod zipcrypto` is unconditional in `zip` 8.6.0 — there is no feature to enable and none in
/// the manifest — so this costs the fixture nothing and proves the same of the reader.
pub fn write_encrypted_archive(path: &Path, entries: &[(&str, Vec<u8>)], password: &str) {
    use zip::unstable::write::FileOptionsExt;

    let file = File::create(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let mut writer = ZipWriter::new(file);
    for (name, bytes) in entries {
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(false)
            .with_deprecated_encryption(password.as_bytes())
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        writer
            .start_file(*name, options)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        writer
            .write_all(bytes)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
    }
    writer
        .finish()
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
}
