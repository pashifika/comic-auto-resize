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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use comic_auto_resize::page::{Channels, EncodeSettings, PageImage, encode};
use flate2::write::DeflateEncoder;
use flate2::{Compression, Crc};
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

/// Every entry of a zip, in stored order, as `(name, bytes)`.
pub fn read_archive(path: &Path) -> Vec<(String, Vec<u8>)> {
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let mut source = comic_auto_resize::source::Source::zip(std::io::Cursor::new(bytes))
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let mut entries = Vec::new();
    while let Some(entry) = source.next_entry() {
        let entry = entry.unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        entries.push((entry.name, entry.bytes));
    }
    entries
}
