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

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use comic_auto_resize::page::{Channels, EncodeSettings, PageImage, encode};
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
/// `large_file(false)` so each local header carries real sizes, which is what a sequential
/// reader needs.
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
    let mut source = comic_auto_resize::source::Source::zip(std::io::Cursor::new(bytes));
    let mut entries = Vec::new();
    while let Some(entry) = source.next_entry() {
        let entry = entry.unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        entries.push((entry.name, entry.bytes));
    }
    entries
}
