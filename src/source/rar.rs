//! Ordered, single-pass rar reading, through `UnRAR`'s header cursor.
//!
//! Unlike zip there is no entry table to read up front. `unrar` walks headers as it goes, and
//! that is not a limitation being worked around: a solid archive cannot be addressed by index
//! at all, so reading start to finish is the only discipline the format allows. What it costs
//! is that a malformed *entry* header surfaces at the first [`RarSource::next_entry`] rather
//! than at open, which [`super::Source::rar`] records.
//!
//! The reader is handed a path rather than a byte stream, because `unrar` is a binding to a
//! C++ library whose entry point takes one (`archive.rs`, storing `Cow<'a, Path>`; the FFI
//! beneath takes `*const wchar_t`).
//!
//! # Entry-name bytes do not survive, which Change 4 inherits
//!
//! Recorded here because this is where someone doing charset work will be looking.
//!
//! The name is converted to a wide string inside the DLL before Rust sees anything
//! (`dll.cpp:247-248`). For a RAR4 archive carrying no Unicode name extension, `arcread.cpp`
//! reaches `ArcCharToWide(…, ACTW_OEM)` — a locale-dependent decode, committed in C++. So the
//! raw stored bytes are already gone by the time this module has a name, and a `--charset`
//! flag could not re-decode them for that class of archive. It is not merely harder than for
//! zip; for RAR4-without-Unicode-names it is not recoverable without changing this dependency.
//!
//! Latent rather than live today: both real samples are pure UTF-8.

use std::path::Path;

use unrar_ng::{
    CursorBeforeHeader, DataSink, OpenArchive, Process,
    error::{Code, When},
};

use super::probe::{self, MAGIC_MAX};
use super::{Entry, HINT_CEILING, MAX_ENTRY_BYTES, SourceError, is_directory, unsafe_name};

/// An archive being walked once, header by header.
pub struct RarSource {
    /// `unrar`'s cursor is a consuming type-state machine — `read_header(self)`,
    /// `read_into(self, …)`, `skip(self)` all take `self` by value — and `next_entry` has
    /// only `&mut self`. So the cursor is `take`n for each step and the successor put back.
    ///
    /// `None` is reachable only if a step panics between the take and the put-back. That path
    /// already has an owner: `pipeline::run` catches a reader-thread panic and reports
    /// `StagePanicked`. So `None` here means "a previous step did not complete" and yields
    /// `None` — the end of the archive — rather than being unwrapped. Written down because an
    /// `Option` that is always `Some` in practice invites an `expect()`.
    archive: Option<OpenArchive<Process, CursorBeforeHeader>>,
    /// Position in the sequence of *yielded* entries, so the writer's key has no gaps where
    /// the archive held something that was not a page.
    next_index: u32,
}

impl RarSource {
    /// Opens the archive at `path`, reading its archive header only.
    ///
    /// # Errors
    ///
    /// [`SourceError::UnsafePath`] when `path` holds an interior NUL, and
    /// [`SourceError::Rar`] when the archive header cannot be read.
    pub fn open(path: &Path) -> Result<Self, SourceError> {
        // Before the path reaches `unrar`, which panics rather than erroring on an interior
        // NUL: `open_for_processing` is documented `# Panics`, via
        // `WideCString::from_os_str(path).expect("Unexpected nul in path")`. A command-line
        // argument cannot carry one on either release target, but the library API is public,
        // and a panic in the reader thread costs the run its message.
        if path.to_string_lossy().contains('\0') {
            return Err(SourceError::UnsafePath);
        }

        Ok(Self {
            archive: Some(unrar_ng::Archive::new(path).open_for_processing()?),
            next_index: 0,
        })
    }

    /// The next page, or `None` at the end of the archive.
    pub fn next_entry(&mut self) -> Option<Result<Entry, SourceError>> {
        loop {
            let archive = self.archive.take()?;

            let header = match archive.read_header() {
                Ok(Some(header)) => header,
                // The archive is consumed by `read_header`, so there is nothing to put back.
                Ok(None) => return None,
                Err(error) => return Some(Err(error.into())),
            };

            // Owned before the header is consumed by `skip` or `read_into`.
            let name = header.entry().filename.to_string_lossy().into_owned();

            // The order of these checks decides which error a multiply-wrong entry gets, so it
            // is stated rather than left to fall out of the code:
            //
            //   1. the directory flag      — not a page at all, so no page diagnosis applies
            //   2. the split flag          — a property of the *archive*, not of this entry
            //   3. the trailing separator  — a directory that failed to set its flag
            //   4. the extension filter    — not a page, and cheap to decide from the name
            //   5. the recorded size       — refusable before any data is read
            //   6. the bytes               — bounded on the way in, then probed
            //
            // 1 before 4 is the point of the flag: `SourceError::Mismatch` ("named as JPEG but
            // its leading bytes are not") would be the wrong diagnosis for a directory. 2 that
            // early because a volume set is refused whatever the entry happens to be — the
            // failure is that the input is one part of a set, not that some page is odd.
            if header.entry().is_directory() {
                match header.skip() {
                    Ok(next) => {
                        self.archive = Some(next);
                        continue;
                    }
                    Err(error) => return Some(Err(error.into())),
                }
            }

            if header.entry().is_split_before() || header.entry().is_split_after() {
                return Some(Err(SourceError::Split { name }));
            }

            // One lookup, so there is no second call to disagree with the first and no
            // `expect` to justify. A directory that failed to set its flag and an extension no
            // candidate claims are the same answer: not a page, pass over it.
            let declared = match probe::declared_format(&name) {
                Some(declared) if !is_directory(&name) => declared,
                _ => match header.skip() {
                    Ok(next) => {
                        self.archive = Some(next);
                        continue;
                    }
                    Err(error) => return Some(Err(error.into())),
                },
            };

            // The header records the size away from the data, so an entry claiming more than
            // the limit costs nothing to refuse and is not read at all.
            let recorded = header.entry().unpacked_size;
            if recorded > MAX_ENTRY_BYTES {
                return Some(Err(SourceError::TooLarge {
                    name,
                    limit: MAX_ENTRY_BYTES,
                }));
            }

            let index = self.next_index;

            // Checking the recorded size is not enough on its own, exactly as for zip: a
            // recorded size that disagrees with what the entry actually holds is the malformed
            // case, and a check that trusts the number it is validating is not a check. The
            // sink refuses the chunk that would take it past the limit, which aborts the read
            // inside libunrar rather than after it.
            //
            // The hint is capped for the same reason `zip.rs` caps it: a hundred-byte entry
            // could record 64 MiB and get 64 MiB reserved, and up to `2 * jobs` of those
            // buffers are alive at once.
            let hint = usize::try_from(recorded.min(HINT_CEILING)).unwrap_or(0);
            let sink = BoundedSink {
                bytes: Vec::with_capacity(hint),
                limit: MAX_ENTRY_BYTES,
            };

            let (sink, result) = header.read_into(sink);
            match result {
                Ok(next) => self.archive = Some(next),
                // The sink refused, so the entry holds more than it declared. The archive is
                // not returned on this path and is not wanted: one page that fails ends the
                // run.
                Err(error) if error.code == Code::Aborted && error.when == When::Process => {
                    return Some(Err(SourceError::TooLarge {
                        name,
                        limit: MAX_ENTRY_BYTES,
                    }));
                }
                Err(error) => return Some(Err(error.into())),
            }
            let bytes = sink.bytes;

            // Probed after the read rather than before it, which is the one place this reader
            // cannot mirror `zip.rs`. There the entry is a reader the caller drives, so the
            // magic can be read on its own; here libunrar pushes the whole entry through the
            // callback. The entry is bounded either way, so the cost of a mismatched entry is
            // one bounded read before the run ends.
            match probe::probe(&bytes[..bytes.len().min(MAGIC_MAX)]) {
                Some(format) if format == declared => {}
                _ => {
                    return Some(Err(SourceError::Mismatch {
                        name,
                        declared: declared.name(),
                    }));
                }
            }

            // The stored name goes into the *output* archive, so a traversing or absolute name
            // would be carried to whatever extracts it. Rejected rather than sanitised.
            if let Some(reason) = unsafe_name(&name) {
                return Some(Err(SourceError::UnsafeName { name, reason }));
            }

            self.next_index += 1;
            return Some(Ok(Entry {
                index,
                name: probe::output_name(&name, declared),
                format: declared,
                bytes,
            }));
        }
    }
}

/// Accumulates an entry, refusing the chunk that would take it past `limit`.
///
/// Returning `false` aborts the read inside libunrar (`vendor/unrar/rdwrfn.cpp` exits on a
/// callback returning `-1` for `UCM_PROCESSDATA`), which is what makes the read-side bound
/// real rather than a check applied after the damage.
struct BoundedSink {
    bytes: Vec<u8>,
    limit: u64,
}

impl DataSink for BoundedSink {
    fn write_chunk(&mut self, chunk: &[u8]) -> bool {
        // Refuses only what would exceed the limit, so an entry of exactly `limit` bytes is
        // accepted — the same boundary `zip.rs` draws.
        if self.bytes.len() as u64 + chunk.len() as u64 > self.limit {
            return false;
        }
        self.bytes.extend_from_slice(chunk);
        true
    }
}

impl std::fmt::Debug for BoundedSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `ProcessMode::Output` requires `Debug`; the bytes are a page and have no business
        // in a log line.
        f.debug_struct("BoundedSink")
            .field("taken", &self.bytes.len())
            .field("limit", &self.limit)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink(limit: u64) -> BoundedSink {
        BoundedSink {
            bytes: Vec::new(),
            limit,
        }
    }

    /// The end-to-end case — a header that declares less than the entry delivers — cannot be
    /// built here, because RARLAB's `rar` is the only RAR writer and it does not write a
    /// header that lies. So the boundary is asserted on the sink, and that the sink's refusal
    /// really stops libunrar is asserted in the dependency's own suite (`tests/data_sink.rs`
    /// in the fork, against both a stored and a solid compressed entry).
    #[test]
    fn a_chunk_that_fits_exactly_is_accepted() {
        let mut sink = sink(4);
        assert!(sink.write_chunk(b"abcd"));
        assert_eq!(sink.bytes, b"abcd");
    }

    #[test]
    fn the_chunk_that_would_exceed_the_limit_is_refused_whole() {
        let mut sink = sink(4);
        assert!(sink.write_chunk(b"abc"));
        assert!(!sink.write_chunk(b"de"), "two more bytes would make five");
        assert_eq!(
            sink.bytes, b"abc",
            "a refused chunk must not be partially taken"
        );
    }

    #[test]
    fn chunks_accumulate_up_to_the_limit() {
        let mut sink = sink(4);
        assert!(sink.write_chunk(b"ab"));
        assert!(sink.write_chunk(b"cd"));
        assert!(!sink.write_chunk(b"e"));
        assert_eq!(sink.bytes, b"abcd");
    }

    /// The limit this reader actually uses, so the boundary is the real one rather than a
    /// convenient small number.
    #[test]
    fn the_real_limit_is_the_shared_one() {
        let mut sink = sink(MAX_ENTRY_BYTES);
        assert!(sink.write_chunk(&[0; 1024]));
        assert!(
            !sink.write_chunk(&vec![
                0;
                usize::try_from(MAX_ENTRY_BYTES).expect("64 MiB fits")
            ]),
            "a chunk past 64 MiB must be refused"
        );
    }

    #[test]
    fn an_empty_chunk_is_accepted_at_the_limit() {
        let mut sink = sink(2);
        assert!(sink.write_chunk(b"ab"));
        assert!(
            sink.write_chunk(b""),
            "libunrar may deliver a final empty chunk"
        );
    }
}
