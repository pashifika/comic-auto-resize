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

use super::probe::{self, MAGIC_MAX};
use super::{Entry, MAX_ENTRY_BYTES, SourceError};

/// How much capacity an entry's recorded size may reserve.
///
/// A real page is tens of kilobytes to a few megabytes, so a megabyte is a useful hint and
/// anything beyond it is the archive's claim rather than a measurement. `read_to_end` grows
/// geometrically from there.
const HINT_CEILING: u64 = 1 << 20;

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
    pub fn new(mut reader: R) -> Result<Self, SourceError> {
        let recorded = recorded_entry_count(&mut reader).map_err(::zip::result::ZipError::Io)?;
        let archive = ZipArchive::new(reader)?;
        if let Some(recorded) = recorded {
            let kept = archive.len() as u64;
            if recorded > kept {
                return Err(SourceError::RepeatedName { recorded, kept });
            }
        }
        Ok(Self {
            archive,
            next_position: 0,
            next_index: 0,
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
            name: probe::output_name(&name, declared),
            format: declared,
            bytes,
        })
    }
}

/// Whether a stored name is a directory rather than a file.
///
/// The archive says so by ending the name with a separator, and both are separators because
/// a Windows-written archive may use either. The same rule `zip` applies internally, spelled
/// out here because its helper is crate-private.
fn is_directory(name: &str) -> bool {
    matches!(name.as_bytes().last(), Some(b'/' | b'\\'))
}

/// Why a stored name must not be carried into the output archive, or `None` if it may be.
///
/// Every check is on the name as the archive stored it, before the extension is rewritten,
/// and both separators are treated as separators because a Windows-written archive may use
/// either.
fn unsafe_name(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("the name is empty");
    }
    if name.contains('\0') {
        return Some("the name contains a NUL byte");
    }
    if name.starts_with('/') || name.starts_with('\\') {
        return Some("the name is absolute");
    }
    // A drive letter (`C:\…`) or a UNC prefix (`\\host\share`), which are absolute on
    // Windows however the leading characters read on a unix host.
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Some("the name carries a drive letter");
    }
    // A component that escapes, in every spelling that resolves to one. Windows strips
    // trailing dots and spaces from a path component, so `.. ` names the parent directory
    // there while an exact comparison against `..` lets it through — and the name is going
    // into an archive somebody will extract on Windows. A component of nothing but dots and
    // spaces is refused whenever it holds two or more dots, which costs `...` and `. .` as
    // well: neither is a page directory, and refusing is the answer this function exists to
    // give.
    if name.split(['/', '\\']).any(|component| {
        let dots = component.bytes().filter(|&byte| byte == b'.').count();
        dots >= 2 && component.bytes().all(|byte| byte == b'.' || byte == b' ')
    }) {
        return Some("the name escapes its own directory");
    }
    None
}

/// What one pass over an entry produced.
enum Yielded {
    Entry(Entry),
    /// Not a page. The entry is left where it is rather than read past.
    Skipped,
    Failed(SourceError),
}

/// Reads up to `buffer.len()` bytes, tolerating short reads.
///
/// Not `read_exact`: an entry shorter than the longest magic is not an I/O error, it is an
/// entry that cannot be a page, and the caller decides that from what was read.
fn fill(reader: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// How many entries the archive's end record says it holds, or `None` when that record does
/// not express the count.
///
/// Not a second archive parser. The end-of-central-directory record is the one structure the
/// format puts at a defined place — last in the file, followed only by its own comment, whose
/// length the record states. A candidate is accepted only when that stated length matches the
/// bytes that follow it, which is what separates this from scanning for a signature that can
/// also occur inside entry data. Searched from the end, as `zip` searches, so a crafted
/// archive is read the same way by both.
///
/// A count of `0xFFFF` means the real one is in the Zip64 end record. That chain is not
/// followed: the count is reported as unknown rather than guessed, so an archive of 65,535
/// entries or more is read without this cross-check. No book has 65,535 pages.
fn recorded_entry_count(reader: &mut (impl Read + Seek)) -> std::io::Result<Option<u64>> {
    /// Signature, four `u16` fields, two `u32`, then the comment length: 22 bytes.
    const RECORD: usize = 22;
    /// The comment length is a `u16`, so the record begins no earlier than this from the end.
    const MAX_TRAILER: u64 = RECORD as u64 + u16::MAX as u64;

    let length = reader.seek(SeekFrom::End(0))?;
    let window = length.min(MAX_TRAILER);
    if window < RECORD as u64 {
        return Ok(None);
    }
    reader.seek(SeekFrom::Start(length - window))?;
    let window = usize::try_from(window).expect("the window is at most 65,557 bytes");
    let mut tail = vec![0; window];
    reader.read_exact(&mut tail)?;

    for start in (0..=window - RECORD).rev() {
        if tail[start..start + END_OF_DIRECTORY.len()] != END_OF_DIRECTORY {
            continue;
        }
        let comment = usize::from(u16::from_le_bytes([tail[start + 20], tail[start + 21]]));
        if start + RECORD + comment != window {
            continue;
        }
        let count = u16::from_le_bytes([tail[start + 10], tail[start + 11]]);
        return Ok((count != u16::MAX).then(|| u64::from(count)));
    }
    Ok(None)
}
