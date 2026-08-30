//! Sequential zip reading.
//!
//! `zip::ZipArchive` offers `by_index` over a `Read + Seek` source, which for zip alone
//! would be simpler. The reader is sequential anyway, for two reasons that outlive zip: a
//! solid rar or 7z archive cannot be accessed randomly at all, so a seeking reader would
//! have to be replaced rather than extended when the source enum gains its second variant;
//! and sequential reading is the pipeline's premise, because a reader that seeks invites the
//! look-ahead that makes peak memory a function of page count.
//!
//! The cost is recorded rather than hidden: an archive written by a streaming writer puts
//! zero sizes in each local header and the real ones in a trailing data descriptor, and
//! `zip` refuses such an entry outright — "The file length is not available in the local
//! header". Go read the central directory and so accepted them. Archives from `WinRAR`,
//! `7-Zip`, and Windows Explorer all carry real local sizes, so the common case is
//! unaffected, and the failure is loud rather than a silently empty entry.

use std::io::{BufRead, Read};

use zip::read::read_zipfile_from_stream;

use super::probe::{self, MAGIC_MAX};
use super::{Entry, MAX_ENTRY_BYTES, SourceError};

/// The local file header every stored entry begins with.
const LOCAL_HEADER: [u8; 4] = [b'P', b'K', 3, 4];

/// Signatures that mean the entries are finished: the central directory, the end of it, and
/// the Zip64 form of the end of it. An archive with no entries at all starts with one of
/// these, which is why the signature is checked rather than assumed.
const TERMINATORS: [[u8; 4]; 3] = [[b'P', b'K', 1, 2], [b'P', b'K', 5, 6], [b'P', b'K', 6, 6]];

/// How much capacity an entry's declared size may reserve.
///
/// A real page is tens of kilobytes to a few megabytes, so a megabyte is a useful hint and
/// anything beyond it is the archive's claim rather than a measurement. `read_to_end` grows
/// geometrically from there.
const HINT_CEILING: u64 = 1 << 20;

/// An archive read once, from start to finish.
pub struct ZipSource<R> {
    reader: R,
    /// Position in the sequence of *yielded* entries, so the writer's key has no gaps where
    /// the archive held something that was not a page.
    next_index: u32,
}

impl<R: BufRead> ZipSource<R> {
    pub const fn new(reader: R) -> Self {
        Self {
            reader,
            next_index: 0,
        }
    }

    /// The next page, or `None` at the end of the archive.
    ///
    /// Entries that are not pages are passed over without being read: a directory, and any
    /// entry whose extension no candidate claims, cost their header alone.
    pub fn next_entry(&mut self) -> Option<Result<Entry, SourceError>> {
        loop {
            match self.read_one() {
                Yielded::Entry(entry) => {
                    self.next_index += 1;
                    return Some(Ok(entry));
                }
                Yielded::Skipped => {}
                Yielded::Done => return None,
                Yielded::Failed(error) => return Some(Err(error)),
            }
        }
    }

    /// One pass over one entry. Separate so the borrow of `self.reader` ends before
    /// `next_entry` returns.
    fn read_one(&mut self) -> Yielded {
        if self.at_end() {
            return Yielded::Done;
        }

        let index = self.next_index;
        let mut file = match read_zipfile_from_stream(&mut self.reader) {
            Ok(Some(file)) => file,
            Ok(None) => return Yielded::Done,
            Err(error) => return Yielded::Failed(SourceError::Archive(error)),
        };

        let name = file.name().to_owned();
        if file.is_dir() {
            return Yielded::Skipped;
        }
        // The extension filter, before any of the entry's data is touched.
        let Some(declared) = probe::declared_format(&name) else {
            return Yielded::Skipped;
        };

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

        // `file.size()` comes from the local header, so it is attacker-controlled. Clamping
        // it to `MAX_ENTRY_BYTES` is not enough on its own: a hundred-byte entry could
        // declare 64 MiB and get 64 MiB reserved, and up to `2 * jobs` of those buffers are
        // alive at once, which on Windows is committed rather than merely reserved. So the
        // hint is capped at a real page's order of magnitude and `read_to_end` grows from
        // there, which it does geometrically anyway.
        let hint = usize::try_from(file.size().min(HINT_CEILING)).unwrap_or(0);
        let mut bytes = Vec::with_capacity(hint.saturating_add(head.len()));
        bytes.extend_from_slice(head);

        // One byte past the limit, so an entry exactly at it is accepted and anything
        // larger is detectable without reading the rest of it.
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

    /// Whether the entries are finished, decided from the next signature without consuming
    /// it.
    ///
    /// `read_zipfile_from_stream` reports the end only when it meets the central directory,
    /// so an archive with no entries — which begins with the end-of-central-directory
    /// record — would otherwise be reported as a malformed local header.
    ///
    /// Returns `false` unless a terminator is positively recognised. `fill_buf` may hand
    /// back fewer than four bytes, and `PK` alone is a prefix of every signature, so an
    /// undecidable peek falls through to `read_zipfile_from_stream` and lets it report the
    /// real problem. An I/O error does the same, because the next read hits it too.
    fn at_end(&mut self) -> bool {
        let Ok(available) = self.reader.fill_buf() else {
            return false;
        };
        if available.is_empty() {
            return true;
        }
        if available.starts_with(&LOCAL_HEADER) {
            return false;
        }
        TERMINATORS
            .iter()
            .any(|terminator| available.starts_with(terminator))
    }
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
    if name.split(['/', '\\']).any(|component| component == "..") {
        return Some("the name escapes its own directory");
    }
    None
}

/// What one pass over an entry produced.
enum Yielded {
    Entry(Entry),
    /// Not a page. Dropping the handle skips the rest of its data.
    Skipped,
    Done,
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
