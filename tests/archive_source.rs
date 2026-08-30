//! Reading an archive as an ordered sequence of named pages.
//!
//! The properties here are the ones the Go implementation got wrong or left to chance:
//! entry order, probe determinism, and a candidate whose declared magic length and compared
//! bytes disagree.

mod support;

use std::io::{Cursor, Write};

use comic_auto_resize::page::{Channels, PageImage};
use comic_auto_resize::page::{EncodeSettings, encode};
use comic_auto_resize::source::{
    CANDIDATES, Format, MAGIC_MAX, MAX_ENTRY_BYTES, Source, SourceError, probe,
};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use support::{Framing, framed_archive};

/// A tiny valid JPEG, encoded rather than committed so the test carries its own input.
fn page(width: u32, height: u32) -> Vec<u8> {
    let pixels = vec![128; (width * height * 3) as usize];
    let image = PageImage::new(width, height, Channels::Rgb, pixels).expect("dimensions match");
    encode("probe.jpg", &image, EncodeSettings::default()).expect("encodes")
}

/// Builds a zip in memory, storing entries in exactly the order given.
///
/// Stored, and `large_file(false)` so no Zip64 extra field appears and the fixture stays a
/// plain 32-bit archive.
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
    let mut source = Source::zip(Cursor::new(bytes))?;
    let mut yielded = Vec::new();
    while let Some(entry) = source.next_entry() {
        let entry = entry?;
        yielded.push((entry.index, entry.name));
    }
    Ok(yielded)
}

/// Every page the source yields, with the number of bytes it produced.
///
/// Separate from `read_all` rather than a widening of it, so the tests that predate the
/// central-directory reader keep asserting exactly what they asserted before.
fn read_all_sized(bytes: &[u8]) -> Result<Vec<(String, usize)>, SourceError> {
    let mut source = Source::zip(Cursor::new(bytes))?;
    let mut yielded = Vec::new();
    while let Some(entry) = source.next_entry() {
        let entry = entry?;
        yielded.push((entry.name, entry.bytes.len()));
    }
    Ok(yielded)
}

/// The names and sizes `entries` should read back as.
fn expected(entries: &[(&str, Vec<u8>)]) -> Vec<(String, usize)> {
    entries
        .iter()
        .map(|(name, data)| ((*name).to_owned(), data.len()))
        .collect()
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
    // The skipped leading entry costs no index. A reader that used the entry table's position
    // as the index — the likely slip now that two counters exist — would yield 1 here and hand
    // the writer a sequence with a gap in it.
    let indices: Vec<_> = yielded.iter().map(|(index, _)| *index).collect();
    assert_eq!(
        indices,
        [0],
        "a skipped entry must not advance the yielded index"
    );
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

/// An archive written the way a writer streaming to a non-seekable output writes one: the
/// local headers record no sizes, the real ones follow each entry in a descriptor and are
/// repeated in the central directory.
#[test]
fn an_entry_whose_size_lives_in_a_data_descriptor_is_read() {
    let entries = [
        ("pages/page01.jpg", page(8, 8)),
        ("pages/page02.jpg", page(64, 96)),
        ("pages/page03.jpg", page(200, 300)),
    ];
    let bytes = framed_archive(
        &entries,
        Framing {
            data_descriptors: true,
            ..Framing::default()
        },
    );

    assert_eq!(
        read_all_sized(&bytes).expect("a streamed archive reads"),
        expected(&entries)
    );
}

/// Which order is meant when the entry table and the entry layout disagree.
#[test]
fn the_central_directory_decides_order_when_the_layout_disagrees() {
    // Distinct page sizes, so this asserts each name arrived with its own data rather than
    // only that the names came out in the right order.
    let entries = [
        ("a.jpg", page(8, 8)),
        ("b.jpg", page(64, 96)),
        ("c.jpg", page(200, 300)),
    ];
    let sizes: Vec<_> = entries.iter().map(|(_, data)| data.len()).collect();
    assert!(
        sizes[0] != sizes[1] && sizes[1] != sizes[2] && sizes[0] != sizes[2],
        "the fixture separates the two orders only if the pages differ in size: {sizes:?}"
    );

    let bytes = framed_archive(
        &entries,
        Framing {
            data_reversed: true,
            ..Framing::default()
        },
    );

    assert_eq!(
        read_all_sized(&bytes).expect("reads"),
        expected(&entries),
        "the central directory lists a, b, c while the data is laid out c, b, a"
    );
}

/// A recorded size past the limit costs nothing to refuse, so the refusal happens before
/// the entry's data is reached.
#[test]
fn an_entry_whose_recorded_size_exceeds_the_limit_is_refused_without_being_read() {
    let past_limit = u32::try_from(MAX_ENTRY_BYTES + 1).expect("the limit is under 4 GiB");
    let entries = [("page01.jpg", page(8, 8))];
    let bytes = framed_archive(
        &entries,
        Framing {
            declared_size: Some(past_limit),
            ..Framing::default()
        },
    );
    // What makes "without being read" observable: the data is not there to read. A reader
    // that went looking for it would fail on the truncation instead of on the limit.
    assert!(
        (bytes.len() as u64) < MAX_ENTRY_BYTES,
        "the fixture must be far smaller than the size it declares"
    );

    let error = read_all(&bytes).expect_err("the recorded size is past the limit");
    assert!(
        matches!(
            &error,
            SourceError::TooLarge { name, limit }
                if name == "page01.jpg" && *limit == MAX_ENTRY_BYTES
        ),
        "expected a size refusal naming the entry and the limit, got {error}"
    );
}

/// A malformed entry is named, which the sequential reader could not do: it met the damage
/// before the name, where the entry table carries every name up front.
#[test]
fn an_entry_the_table_lists_but_cannot_locate_is_named() {
    let entries = [("page01.jpg", page(8, 8)), ("page02.jpg", page(8, 8))];
    let bytes = framed_archive(
        &entries,
        Framing {
            orphaned_entry: Some(1),
            ..Framing::default()
        },
    );

    let error = read_all(&bytes).expect_err("the second entry cannot be located");
    assert!(
        matches!(&error, SourceError::Entry { name, .. } if name == "page02.jpg"),
        "expected an entry failure naming the entry, got {error}"
    );
}

/// Two entries stored under one name are refused, not silently reduced to one.
///
/// `zip` keys its entry table on the stored name, so the second record replaces the first and
/// `len()` counts one. Without the cross-check against what the archive records, the run would
/// write a book one page short and report success.
///
/// The two framings after the plain case are the ways the cross-check was got wrong once, so
/// they are the ways it can silently stop working:
///
/// - The end record states the entry count twice, once for this disk and once in total. `zip`
///   counts records with the first; a reader taking the second compares two independent
///   numbers, and an archive that states them differently escapes the check.
/// - The record's comment is the last thing the format puts in the file, but readers tolerate
///   garbage after it — `zip` deliberately relaxed that check. A reader requiring the comment
///   to end exactly at the end of the file is disabled by one trailing byte.
#[test]
fn two_entries_stored_under_one_name_are_refused() {
    let entries = [
        ("pages/page01.jpg", page(8, 8)),
        ("pages/page01.jpg", page(64, 96)),
        ("pages/page02.jpg", page(8, 8)),
    ];
    for framing in [
        Framing::default(),
        Framing {
            recorded_total: Some(2),
            ..Framing::default()
        },
        Framing {
            recorded_total: Some(u16::MAX),
            ..Framing::default()
        },
        Framing {
            trailing_bytes: 1,
            ..Framing::default()
        },
    ] {
        let bytes = framed_archive(&entries, framing);

        let error = read_all(&bytes).expect_err("a repeated stored name must be refused");
        assert!(
            matches!(
                &error,
                SourceError::RepeatedName { recorded, kept } if *recorded == 3 && *kept == 2
            ),
            "{framing:?}: expected a repeated-name refusal counting both sides, got {error}"
        );
    }
}

/// An entry table the reader can address in full is read, and a collision the extension
/// rewriting creates is the writer's to report rather than the reader's.
///
/// The guard against the cross-check refusing what it should not: two distinct stored names,
/// with the end record's total field lying in the direction that would trip a reader counting
/// with it.
#[test]
fn an_addressable_entry_table_is_not_refused() {
    let entries = [
        ("pages/page01.jpeg", page(8, 8)),
        ("pages/page01.jpg", page(64, 96)),
    ];
    let bytes = framed_archive(
        &entries,
        Framing {
            recorded_total: Some(3),
            ..Framing::default()
        },
    );

    let yielded = read_all_sized(&bytes).expect("an addressable entry table reads");
    assert_eq!(
        yielded.len(),
        2,
        "both entries are addressable, so both are read: {yielded:?}"
    );
}

/// A component Windows normalisation turns into `..` escapes just as `..` does.
#[test]
fn a_name_whose_component_normalises_to_a_parent_is_refused() {
    for stored in [
        "pages/.. /escape.jpg",
        "pages\\.. \\escape.jpg",
        ".../escape.jpg",
    ] {
        let bytes = framed_archive(&[(stored, page(8, 8))], Framing::default());

        let error = read_all(&bytes).expect_err("a traversing name must be refused");
        assert!(
            matches!(
                &error,
                SourceError::UnsafeName { reason, .. }
                    if *reason == "the name escapes its own directory"
            ),
            "{stored}: expected a traversal refusal, got {error}"
        );
    }
}
