//! Reading an archive as an ordered sequence of named pages.
//!
//! The properties here are the ones the Go implementation got wrong or left to chance:
//! entry order, probe determinism, and a candidate whose declared magic length and compared
//! bytes disagree.

use std::io::{Cursor, Write};

use comic_auto_resize::page::{Channels, PageImage};
use comic_auto_resize::page::{EncodeSettings, encode};
use comic_auto_resize::source::{CANDIDATES, Format, MAGIC_MAX, Source, SourceError, probe};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// A tiny valid JPEG, encoded rather than committed so the test carries its own input.
fn page(width: u32, height: u32) -> Vec<u8> {
    let pixels = vec![128; (width * height * 3) as usize];
    let image = PageImage::new(width, height, Channels::Rgb, pixels).expect("dimensions match");
    encode("probe.jpg", &image, EncodeSettings::default()).expect("encodes")
}

/// Builds a zip in memory, storing entries in exactly the order given.
///
/// Stored, and `large_file(false)`, so each local header carries real sizes — which is what
/// a sequential reader needs.
fn archive(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .large_file(false);
    for (name, bytes) in entries {
        writer.start_file(*name, options).expect("starts the entry");
        writer.write_all(bytes).expect("writes the entry");
    }
    writer.finish().expect("finishes").into_inner()
}

/// Every page the source yields, in order.
fn read_all(bytes: &[u8]) -> Result<Vec<(u32, String)>, SourceError> {
    let mut source = Source::zip(Cursor::new(bytes));
    let mut yielded = Vec::new();
    while let Some(entry) = source.next_entry() {
        let entry = entry?;
        yielded.push((entry.index, entry.name));
    }
    Ok(yielded)
}

#[test]
fn entries_arrive_in_stored_order_not_alphabetical_order() {
    // Stored back to front, so alphabetical order and stored order disagree.
    let bytes = archive(&[
        ("c.jpg", page(8, 8)),
        ("a.jpg", page(8, 8)),
        ("b.jpg", page(8, 8)),
    ]);

    let yielded = read_all(&bytes).expect("reads");
    let names: Vec<_> = yielded.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(names, ["c.jpg", "a.jpg", "b.jpg"]);

    // The index is the position in the yielded sequence, with no gaps.
    let indices: Vec<_> = yielded.iter().map(|(index, _)| *index).collect();
    assert_eq!(indices, [0, 1, 2]);
}

#[test]
fn the_same_archive_probes_identically_twice() {
    let bytes = archive(&[("page01.jpg", page(8, 8)), ("page02.jpeg", page(8, 8))]);

    let first = read_all(&bytes).expect("reads");
    let second = read_all(&bytes).expect("reads");
    assert_eq!(first, second);

    // Go iterated a map here, so this is the property that was not guaranteed before.
    for _ in 0..16 {
        assert_eq!(read_all(&bytes).expect("reads"), first);
    }
}

#[test]
fn a_candidates_compared_bytes_are_the_bytes_it_declares() {
    // The `utils/images/plugs/bmp.go` defect: `Matched` compared the webp header while
    // `HeaderLen` returned the bmp header's length, so the candidate could never match.
    // Here one field is both, so the mismatch cannot be expressed.
    for candidate in CANDIDATES {
        assert!(candidate.magic.len() <= MAGIC_MAX);
        assert_eq!(probe(candidate.magic), Some(candidate.format));
    }
}

#[test]
fn a_non_image_entry_is_passed_over() {
    let bytes = archive(&[
        ("ComicInfo.xml", b"<?xml version=\"1.0\"?>".to_vec()),
        ("page01.jpg", page(8, 8)),
        ("Thumbs.db", vec![0; 64]),
    ]);

    let yielded = read_all(&bytes).expect("reads");
    let names: Vec<_> = yielded.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(names, ["page01.jpg"], "only the page is a page");
}

#[test]
fn an_entry_whose_extension_lies_is_an_error() {
    // Named as a JPEG, holding PNG bytes. Go treated this as an error too, because the
    // extension filter had already claimed it was an image.
    let bytes = archive(&[("page01.jpg", b"\x89PNG\r\n\x1a\n".to_vec())]);

    let error = read_all(&bytes).expect_err("the bytes contradict the name");
    assert!(
        matches!(&error, SourceError::Mismatch { name, .. } if name == "page01.jpg"),
        "expected a mismatch naming the entry, got {error}"
    );
}

#[test]
fn a_jpeg_extension_is_renamed_to_the_encoders() {
    let bytes = archive(&[
        ("pages/page01.jpeg", page(8, 8)),
        ("pages/page02.JPG", page(8, 8)),
    ]);

    let yielded = read_all(&bytes).expect("reads");
    let names: Vec<_> = yielded.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(names, ["pages/page01.jpg", "pages/page02.jpg"]);
    assert_eq!(Format::Jpeg.extension(), "jpg");
}

#[test]
fn a_directory_entry_is_not_a_page() {
    let bytes = {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer.add_directory("pages/", options).expect("adds a dir");
        writer
            .start_file("pages/page01.jpg", options)
            .expect("starts");
        writer.write_all(&page(8, 8)).expect("writes");
        writer.finish().expect("finishes").into_inner()
    };

    let yielded = read_all(&bytes).expect("reads");
    let names: Vec<_> = yielded.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(names, ["pages/page01.jpg"]);
}

#[test]
fn an_empty_archive_yields_nothing() {
    assert_eq!(read_all(&archive(&[])).expect("reads"), Vec::new());
}
