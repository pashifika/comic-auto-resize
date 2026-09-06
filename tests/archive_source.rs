//! Reading an archive as an ordered sequence of named pages.
//!
//! The properties here are the ones the Go implementation got wrong or left to chance:
//! entry order, probe determinism, and a candidate whose declared magic length and compared
//! bytes disagree.

mod support;

use std::io::{Cursor, Write};

use comic_auto_resize::page::{Channels, EncodeSettings, Format, PageImage, encode};
use comic_auto_resize::source::{
    CANDIDATES, MAGIC_MAX, MAX_ENTRY_BYTES, ReadOptions, SourceError, ZipSource, probe,
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
    // `ZipSource` rather than `Source`, because these tests are about the zip reader and the
    // enum names `File` for the one reader the binary ever opens. Same code under test.
    let mut source = ZipSource::new(Cursor::new(bytes), &ReadOptions::default())?;
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
    let mut source = ZipSource::new(Cursor::new(bytes), &ReadOptions::default())?;
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
    // `HeaderLen` returned the bmp header's length, so the candidate could never match. Here
    // the length is derived from the fields the comparison reads, so the mismatch cannot be
    // expressed — and with four candidates, no two may claim one header either.
    for candidate in CANDIDATES {
        let magic = &candidate.magic;
        assert!(magic.header_len() <= MAGIC_MAX);

        // From the candidate's own declaration, with a filler no candidate declares in the
        // positions it says are skipped.
        let mut header = magic.head.to_vec();
        header.resize(header.len() + magic.skipped, 0xA5);
        header.extend_from_slice(magic.tail);

        assert_eq!(probe(&header), Some(candidate.format));
        let claimed = CANDIDATES
            .iter()
            .filter(|other| other.magic.matches(&header))
            .count();
        assert_eq!(claimed, 1, "{:?} shares a header", candidate.format);
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

/// An entry that records a modest size and then inflates past the limit is refused too.
///
/// This is why the bound on the read survives alongside the check on the recorded size. `zip`
/// bounds a Stored entry by the size the entry table records, but it hands a Deflate entry to
/// `flate2` without that bound, so the stream decides how many bytes arrive. An archive of
/// 66 kilobytes can therefore deliver 64 mebibytes, and the recorded-size check waves it
/// through because the record is modest.
///
/// Costs one buffer past the limit while it runs, which is the smallest fixture that can reach
/// the branch at all: the limit is what is being tested.
#[test]
fn an_entry_that_inflates_past_the_limit_is_refused() {
    let head = page(8, 8);
    let mut payload = head.clone();
    payload.resize(
        usize::try_from(MAX_ENTRY_BYTES).expect("the limit fits") + 1,
        0,
    );
    let entries = [("page01.jpg", payload)];

    let bytes = framed_archive(
        &entries,
        Framing {
            deflated: true,
            declared_size: Some(4096),
            ..Framing::default()
        },
    );
    assert!(
        (bytes.len() as u64) < MAX_ENTRY_BYTES / 64,
        "the fixture must be orders of magnitude smaller than what it inflates to: {} B",
        bytes.len()
    );

    let error = read_all(&bytes).expect_err("an entry past the limit is refused");
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
///
/// It is also the one entry whose name is *not* the decoded one, because a record pointing at
/// no local header yields no stored name bytes either — so the refusal says which decode the
/// name came from. In an archive read under a chosen encoding, every other entry is reported as
/// the characters that encoding names, and an unremarked CP437 name among them would send the
/// reader looking for a page that exists under no such name.
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
        matches!(&error, SourceError::Unreachable { name, .. } if name == "page02.jpg"),
        "expected an unreachable-entry failure naming the entry, got {error}"
    );
    let message = error.to_string();
    assert!(
        message.contains("container's own decoding"),
        "the refusal must say the name is not the decoded one: {message}"
    );
}

/// Two entries stored under one name are refused, not silently reduced to one.
///
/// `zip` keys its entry table on the stored name, so the second record replaces the first and
/// `len()` counts one. Without the cross-check against what the archive records, the run would
/// write a book one page short and report success.
///
/// The framings after the plain case are the ways the cross-check was got wrong once, so they
/// are the ways it can silently stop working:
///
/// - The end record states the entry count twice, once for this disk and once in total. `zip`
///   counts records with the first; a reader taking the second compares two independent
///   numbers, and an archive that states them differently escapes the check.
/// - The record's comment is the last thing the format puts in the file, but readers tolerate
///   garbage after it — `zip` deliberately relaxed that check. A reader requiring the comment
///   to end exactly at the end of the file is disabled by one trailing byte.
/// - The comment may be 65,535 bytes long, which puts the record that far from the end and the
///   bytes *before* the record further still. A reader whose search window has no room for
///   what precedes the record cannot establish the record's Zip64 status, and gives up on a
///   perfectly conformant archive.
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
        Framing {
            comment_bytes: usize::from(u16::MAX),
            ..Framing::default()
        },
        // The shortest comment the window before this one failed on: it reached the record at
        // `65,535 - comment`, so it lost the locator's bytes from 65,516 upwards.
        Framing {
            comment_bytes: usize::from(u16::MAX) - 19,
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

/// What one pass over a reader cost.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Cost {
    bytes: u64,
    seeks: u64,
}

/// A reader that counts what is drawn through it.
struct Counting<R> {
    inner: R,
    cost: std::rc::Rc<std::cell::Cell<Cost>>,
}

impl<R: std::io::Read> std::io::Read for Counting<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        let mut cost = self.cost.get();
        cost.bytes += read as u64;
        self.cost.set(cost);
        Ok(read)
    }
}

impl<R: std::io::Seek> std::io::Seek for Counting<R> {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        let mut cost = self.cost.get();
        cost.seeks += 1;
        self.cost.set(cost);
        self.inner.seek(to)
    }
}

/// What `work` drew through a counting reader over `bytes`.
fn counted(bytes: &[u8], work: impl FnOnce(Counting<Cursor<&[u8]>>)) -> Cost {
    let cost = std::rc::Rc::new(std::cell::Cell::new(Cost::default()));
    work(Counting {
        inner: Cursor::new(bytes),
        cost: std::rc::Rc::clone(&cost),
    });
    cost.get()
}

/// What locating an entry to read its stored name costs, measured rather than argued.
///
/// The reader needs every entry's name *bytes* before the first page, because the encoding a
/// container declares nothing about is chosen for the whole container — and those bytes are
/// reachable only from a located entry. So each entry is located once when the archive is
/// opened, and the requirement is that locating reads no entry data and leaves the pipeline's
/// bound alone.
///
/// Measured here as the difference between reading the entry table and reading it plus
/// locating every entry, which isolates exactly the call the survey adds.
#[test]
fn locating_an_entry_to_read_its_name_reads_no_entry_data() {
    // Non-pages, so the extension filter drops every one of them: whatever this reads, it is
    // not a page being read on purpose. A quarter-megabyte each, so touching even one entry's
    // data would be unmistakable against the bound below.
    let payload = vec![b'x'; 256 * 1024];
    let entries: Vec<(&str, Vec<u8>)> = ["a.xml", "b.xml", "c.xml", "d.xml", "e.xml", "f.xml"]
        .into_iter()
        .map(|name| (name, payload.clone()))
        .collect();
    let bytes = archive(&entries);
    let count = entries.len() as u64;

    let table_only = counted(&bytes, |reader| {
        zip::ZipArchive::new(reader).expect("the entry table reads");
    });
    let table_and_locate = counted(&bytes, |reader| {
        let mut archive = zip::ZipArchive::new(reader).expect("the entry table reads");
        for position in 0..archive.len() {
            archive.by_index_raw(position).expect("locates the entry");
        }
    });

    // Thirty bytes: the local header's four-byte signature and its twenty-six-byte fixed
    // block, from which the data offset is computed arithmetically rather than by scanning.
    // Two seeks: one to the local header, one to the data it just located.
    assert_eq!((table_and_locate.bytes - table_only.bytes) / count, 30);
    assert_eq!((table_and_locate.seeks - table_only.seeks) / count, 2);

    // And the reader as a whole reads no entry data for an entry it passes over. The fixed
    // cost it is bounded against is the backwards search for the end-of-directory record,
    // which is at most 65,577 bytes and predates this Change.
    let source = counted(&bytes, |reader| {
        let mut source = ZipSource::new(reader, &ReadOptions::default()).expect("opens");
        assert!(source.next_entry().is_none(), "no entry is a page");
    });
    assert!(
        source.bytes < 128 * 1024,
        "the reader drew {} B over an archive of six {} B entries, so it read entry data",
        source.bytes,
        payload.len()
    );
}
