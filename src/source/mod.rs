//! Reading an archive as an ordered sequence of named pages.
//!
//! An enum rather than trait objects: the set of archive formats is closed and known at
//! compile time, so a `match` costs nothing and every variant's reading discipline stays
//! visible. zip is the only variant here; rar, 7z, and a plain directory arrive later.

mod probe;
mod zip;

pub use probe::{CANDIDATES, Candidate, Format, MAGIC_MAX, declared_format, output_name, probe};

use std::io::BufRead;

use thiserror::Error;

/// The most bytes one entry may occupy in memory.
///
/// An entry's declared size comes from the archive, so it is attacker-controlled, and a
/// small archive can declare an enormous entry. The page budget cannot help here: it
/// governs decoded pixels, and these bytes are read before anything is decoded.
///
/// Chosen, not measured. A 1280-wide JPEG page is tens of kilobytes and a 600dpi scan a few
/// megabytes, so 64 MiB is orders of magnitude of headroom while still refusing an archive
/// that claims a gigabyte in one entry.
const MAX_ENTRY_BYTES: u64 = 64 << 20;

/// One page read out of an archive.
pub struct Entry {
    /// Position in the sequence of yielded pages, from zero. The writer orders on it.
    pub index: u32,
    /// The name the output entry is written under: the stored name with its extension
    /// replaced by the encoder's.
    pub name: String,
    pub format: Format,
    pub bytes: Vec<u8>,
}

/// An archive being read once, from start to finish.
pub enum Source<R> {
    Zip(zip::ZipSource<R>),
}

impl<R: BufRead> Source<R> {
    pub const fn zip(reader: R) -> Self {
        Self::Zip(zip::ZipSource::new(reader))
    }

    /// The next page, or `None` at the end of the archive.
    ///
    /// Nothing is retained after it is returned, which is what lets an archive larger than
    /// memory be processed.
    pub fn next_entry(&mut self) -> Option<Result<Entry, SourceError>> {
        match self {
            Self::Zip(source) => source.next_entry(),
        }
    }
}

/// Why an archive could not be read.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SourceError {
    /// The archive's own structure. Carries no entry name because `zip` reports a malformed
    /// local header before the name is available — including the refusal of an entry whose
    /// sizes live in a trailing data descriptor.
    #[error("cannot read the archive: {0}")]
    Archive(#[from] ::zip::result::ZipError),
    #[error("{name}: cannot read the entry: {source}")]
    Entry {
        name: String,
        source: std::io::Error,
    },
    /// The extension claimed a format the leading bytes contradict. An error rather than a
    /// skip, because the archive is inconsistent and silently dropping the page would
    /// shorten the book.
    #[error("{name}: named as {declared} but its leading bytes are not {declared}")]
    Mismatch {
        name: String,
        declared: &'static str,
    },
    #[error("{name}: entry is larger than the limit of {limit} bytes")]
    TooLarge { name: String, limit: u64 },
    /// The stored name would be carried into the output archive, where a traversing or
    /// absolute name is a hazard for whatever extracts it. Rejected rather than sanitised,
    /// because rewriting it would produce an output whose entries do not match the input's.
    #[error("{name}: refusing the entry name because {reason}")]
    UnsafeName { name: String, reason: &'static str },
}
