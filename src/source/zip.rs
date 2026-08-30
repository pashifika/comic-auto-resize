//! Ordered, single-pass zip reading, through the archive's central directory.
//!
//! `ZipArchive::new` reads the entry table once and `by_index` then reads one entry at a
//! time, in that table's order. The table is where the format records each entry's real
//! size, so an archive written by a writer streaming to a non-seekable output — zero sizes
//! and general-purpose flag bit 3 in every local header, the real sizes in a trailing data
//! descriptor — reads like any other. Go read the central directory too.
//!
//! This replaces `read_zipfile_from_stream`, which takes each size from the local header and
//! so refuses such an entry outright: "The file length is not available in the local
//! header". The reader was sequential deliberately, on the grounds that one able to seek
//! invites the look-ahead that makes peak memory a function of page count. That is answered
//! elsewhere now: the pipeline's credit window bounds the entries in flight however the
//! reader obtained them, so restraint is no longer the reader's job to exercise.
//!
//! The sequential discipline is gone from zip rather than from the design. A solid rar or 7z
//! archive cannot be addressed by index at all, so the source enum will hold both.
//!
//! The entry table is also the authority on order. A malformed archive can lay its data out
//! in one order and list it in another; the table's order is the one every other reader
//! presents, including the viewer the output will be opened in.
//!
//! One thing the table does not survive intact. `ZipArchive` keys it on the stored name, so
//! two entries stored under one name byte for byte collapse into a single record and the
//! loser would leave the book without a word. The end record counts what the archive says it
//! holds; a disagreement with what the table kept is refused rather than shortened.

use std::io::{Read, Seek, SeekFrom};

use zip::ZipArchive;

use super::probe::{self, MAGIC_MAX, Names, Naming};
use super::{Entry, HINT_CEILING, MAX_ENTRY_BYTES, SourceError, fill, is_directory, unsafe_name};

/// The signature of the 32-bit end-of-central-directory record.
const END_OF_DIRECTORY: [u8; 4] = [b'P', b'K', 5, 6];

/// An archive whose entry table has been read, walked once in that table's order.
pub struct ZipSource<R> {
    archive: ZipArchive<R>,
    /// The next position in the entry table.
    next_position: usize,
    /// Position in the sequence of *yielded* entries, so the writer's key has no gaps where
    /// the archive held something that was not a page.
    next_index: u32,
    names: Names,
}

impl<R: Read + Seek> ZipSource<R> {
    /// Reads the archive's entry table.
    ///
    /// # Errors
    ///
    /// [`SourceError::Archive`] when the entry table cannot be read. It is read here, before
    /// any entry, so a truncated or malformed one fails the run before a page is processed.
    /// [`SourceError::RepeatedName`] when the table kept fewer entries than the archive
    /// records, which is what two entries stored under one name look like from here.
    pub fn new(mut reader: R, naming: Naming) -> Result<Self, SourceError> {
        let recorded = recorded_entry_count(&mut reader).map_err(::zip::result::ZipError::Io)?;
        let archive = ZipArchive::new(reader)?;
        if let Some(recorded) = recorded {
            let kept = archive.len() as u64;
            if recorded > kept {
                return Err(SourceError::RepeatedName { recorded, kept });
            }
        }
        let names = match naming {
            Naming::Stored => Names::stored(),
            // The entry table is read by now, so the entry total costs nothing here.
            Naming::ByPosition => Names::by_position(archive.len()),
        };
        Ok(Self {
            archive,
            next_position: 0,
            next_index: 0,
            names,
        })
    }

    /// The next page, or `None` at the end of the archive.
    ///
    /// Entries that are not pages are passed over without being reached at all: a directory,
    /// and any entry whose extension no candidate claims, cost nothing beyond the name the
    /// entry table already holds.
    pub fn next_entry(&mut self) -> Option<Result<Entry, SourceError>> {
        while self.next_position < self.archive.len() {
            let position = self.next_position;
            self.next_position += 1;
            match self.read_one(position) {
                Yielded::Entry(entry) => {
                    self.next_index += 1;
                    return Some(Ok(entry));
                }
                Yielded::Skipped => {}
                Yielded::Failed(error) => return Some(Err(error)),
            }
        }
        None
    }

    /// One pass over the entry at `position` in the table.
    fn read_one(&mut self, position: usize) -> Yielded {
        let Some(name) = self.archive.name_for_index(position) else {
            // `position` came from the archive's own entry count, so this is unreachable.
            // Reported rather than asserted: the reader runs on its own thread, where a
            // panic costs the run its message and buys nothing.
            return Yielded::Failed(SourceError::Archive(::zip::result::ZipError::FileNotFound));
        };
        if is_directory(name) {
            return Yielded::Skipped;
        }
        // The extension filter, before the entry is located, let alone read.
        let Some(declared) = probe::declared_format(name) else {
            return Yielded::Skipped;
        };
        // Ends the borrow of the entry table, which reading the entry needs mutably.
        let name = name.to_owned();

        let index = self.next_index;
        let mut file = match self.archive.by_index(position) {
            Ok(file) => file,
            // Named, which the sequential reader could not do: it met a malformed entry
            // before its name, where the entry table carries the name of every entry
            // whether or not the entry itself can be read.
            Err(error) => {
                return Yielded::Failed(SourceError::Entry {
                    name,
                    source: error.into(),
                });
            }
        };

        // The recorded size is still the archive's claim, but the entry table records it
        // away from the entry's data, so it is known before that data is reached: an entry
        // claiming more than the limit costs nothing to refuse and is not read at all.
        if file.size() > MAX_ENTRY_BYTES {
            return Yielded::Failed(SourceError::TooLarge {
                name,
                limit: MAX_ENTRY_BYTES,
            });
        }

        let mut head = [0; MAGIC_MAX];
        let head = match fill(&mut file, &mut head) {
            Ok(read) => &head[..read],
            Err(source) => return Yielded::Failed(SourceError::Entry { name, source }),
        };

        // The extension said this was a page. If the bytes disagree the archive is
        // inconsistent, which Go also treated as an error rather than a skip.
        match probe::probe(head) {
            Some(format) if format == declared => {}
            _ => {
                return Yielded::Failed(SourceError::Mismatch {
                    name,
                    declared: declared.name(),
                });
            }
        }

        // Checking the recorded size is not enough on its own: a hundred-byte entry could
        // record 64 MiB and get 64 MiB reserved, and up to `2 * jobs` of those buffers are
        // alive at once, which on Windows is committed rather than merely reserved. So the
        // hint is capped at a real page's order of magnitude and `read_to_end` grows from
        // there, which it does geometrically anyway.
        let hint = usize::try_from(file.size().min(HINT_CEILING)).unwrap_or(0);
        let mut bytes = Vec::with_capacity(hint.saturating_add(head.len()));
        bytes.extend_from_slice(head);

        // The bound on the read stays, rather than trusting the size just checked: a
        // recorded size that disagrees with what the entry actually holds is precisely the
        // malformed case, and a check that trusts the number it is validating is not a
        // check. One byte past the limit, so an entry exactly at it is accepted and
        // anything larger is detectable without reading the rest of it.
        let remaining = MAX_ENTRY_BYTES
            .saturating_sub(bytes.len() as u64)
            .saturating_add(1);
        if let Err(source) = file.take(remaining).read_to_end(&mut bytes) {
            return Yielded::Failed(SourceError::Entry { name, source });
        }
        if bytes.len() as u64 > MAX_ENTRY_BYTES {
            return Yielded::Failed(SourceError::TooLarge {
                name,
                limit: MAX_ENTRY_BYTES,
            });
        }

        // The stored name goes into the *output* archive, so a traversing or absolute name
        // would be carried to whatever extracts it. `zip`'s own documentation warns that
        // `ZipFile::name` may be absolute or escape via `..`. Rejected rather than
        // sanitised: silently rewriting a name would produce an archive whose entries do not
        // match the input's, which is worse than refusing.
        if let Some(reason) = unsafe_name(&name) {
            return Yielded::Failed(SourceError::UnsafeName { name, reason });
        }

        Yielded::Entry(Entry {
            index,
            name: self.names.of(&name, declared),
            format: declared,
            bytes,
        })
    }
}

/// What one pass over an entry produced.
enum Yielded {
    Entry(Entry),
    /// Not a page. The entry is left where it is rather than read past.
    Skipped,
    Failed(SourceError),
}

/// How many entries the archive's end record says it holds, or `None` when that cannot be
/// established from the 32-bit end record alone.
///
/// Not a second archive parser, and deliberately not a general one: it locates the single
/// record the format puts at a defined place — last in the file, followed only by its own
/// comment — and reads one field from it under the rules `zip` applies to the same record.
/// Every disagreement resolves to `None`, so the cross-check is skipped rather than a valid
/// archive refused.
///
/// The rules, each mirroring the dependency or narrowing safely:
///
/// - Searched from the end, as `zip`'s backwards finder searches.
/// - The comment must end at or before the end of the file, not exactly at it. `zip` relaxed
///   that check for archives carrying garbage after the comment, so requiring an exact fit
///   would let one trailing byte silence the cross-check.
/// - The directory must end where the record begins. `zip` does not check this; it is what
///   stops a `PK\x05\x06` inside entry data from passing for the real record, and it also
///   excludes an archive with data prepended, whose recorded offsets are relative to the
///   archive rather than to the file.
/// - The count is the entries *on this disk*, at offset 8, because that is the field `zip`
///   bounds its record loop with. Offset 10 never bounds that loop: it reaches `zip` as one of
///   three Zip64 hints in `may_be_zip64`, and as the empty-archive short-circuit in the zip32
///   branch, and neither is a count of records read. Counting with it would compare two
///   independent numbers.
/// - A Zip64 locator immediately before the record, or a count of `0xFFFF`, means the
///   authoritative count is in the Zip64 end record. That chain is not followed: no book has
///   65,535 pages, and a second footer parser is the thing this function exists not to be.
///
/// The search covers the last 65,577 bytes: the locator that may precede the record, the
/// record, and the longest comment the format allows after it — a record whose preceding bytes
/// cannot be read is a record whose Zip64 status cannot be established, so the window has to
/// hold both. Within it a record begins at `65,555 - comment - trailing`, so the count is
/// established for every conformant archive, whose comment is at most 65,535 bytes with
/// nothing after it, and given up on when comment and trailing garbage together exceed 65,535.
/// `zip` searches the whole file, so such an archive is read by `zip` and not cross-checked
/// here. It is not conformant, and the check exists to catch an archive that lost an entry by
/// accident rather than one built to lose one — scanning a 300 MB file backwards for a
/// signature would be its own guess about where to stop.
fn recorded_entry_count(reader: &mut (impl Read + Seek)) -> std::io::Result<Option<u64>> {
    /// Signature, four `u16` fields, two `u32`, then the comment length: 22 bytes.
    const RECORD: usize = 22;
    /// The Zip64 end-of-central-directory locator, which sits immediately before the 32-bit
    /// record when the archive is Zip64.
    const ZIP64_LOCATOR: [u8; 4] = [b'P', b'K', 6, 7];
    /// The locator's signature plus its fixed block.
    const LOCATOR: usize = 20;
    /// The comment length is a `u16` and the locator that may precede the record is another
    /// 20 bytes, so the window begins no earlier than this from the end.
    const MAX_TRAILER: u64 = RECORD as u64 + u16::MAX as u64 + LOCATOR as u64;

    let length = reader.seek(SeekFrom::End(0))?;
    let window = length.min(MAX_TRAILER);
    if window < RECORD as u64 {
        return Ok(None);
    }
    let base = length - window;
    reader.seek(SeekFrom::Start(base))?;
    let window = usize::try_from(window).expect("the window is at most 65,577 bytes");
    let mut tail = vec![0; window];
    reader.read_exact(&mut tail)?;

    for start in (0..=window - RECORD).rev() {
        let record = &tail[start..];
        if record[..END_OF_DIRECTORY.len()] != END_OF_DIRECTORY {
            continue;
        }
        let comment = usize::from(u16::from_le_bytes([record[20], record[21]]));
        if start + RECORD + comment > window {
            continue;
        }
        let size = u32::from_le_bytes([record[12], record[13], record[14], record[15]]);
        let offset = u32::from_le_bytes([record[16], record[17], record[18], record[19]]);
        if u64::from(offset) + u64::from(size) != base + start as u64 {
            continue;
        }

        let zip64 = match start.checked_sub(LOCATOR) {
            Some(at) => tail[at..at + ZIP64_LOCATOR.len()] == ZIP64_LOCATOR,
            // The bytes before the record are outside the window, so the locator's absence
            // cannot be established. The window holds the locator ahead of the longest comment
            // the format allows, so this needs comment plus trailing garbage over 65,535 —
            // which no conformant archive has.
            None if base > 0 => return Ok(None),
            None => false,
        };
        if zip64 {
            return Ok(None);
        }

        let count = u16::from_le_bytes([record[8], record[9]]);
        return Ok((count != u16::MAX).then(|| u64::from(count)));
    }
    Ok(None)
}
