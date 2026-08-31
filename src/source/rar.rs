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
//!
//! # A truncated archive is indistinguishable from a complete one
//!
//! `RARReadHeaderEx` returns `ERAR_END_ARCHIVE` both at a genuine end and at a clean EOF
//! part-way through the header chain: `dll.cpp` reaches that return when `SearchBlock`
//! finds no further file header and `BrokenHeader` is false, which is exactly what a cut-off
//! download looks like. So a truncated rar is reported as an archive that yielded no page,
//! not as a damaged one, and RARLAB's own `unrar l` says `Unexpected end of archive` for the
//! same input.
//!
//! Not recoverable through this dependency — the distinction is discarded inside the DLL, not
//! here — so `RunError::Empty` says the archive *yielded* no image entry rather than that it
//! holds none. Fixing it properly means a patch to the vendored C++ to report the missing
//! end-of-archive record, which is the fork's job and not this Change's.

use std::path::Path;

use unrar_ng::{
    CursorBeforeHeader, DataSink, OpenArchive, Process,
    error::{Code, When},
};

use super::probe::{self, MAGIC_MAX, Names, Naming};
use super::{
    Entry, HINT_CEILING, MAX_ENTRY_BYTES, ReadOptions, SourceError, is_directory, unsafe_name,
};

/// An archive being walked once, header by header.
pub struct RarSource {
    /// `unrar`'s cursor is a consuming type-state machine — `read_header(self)`,
    /// `read_into(self, …)`, `skip(self)` all take `self` by value — and `next_entry` has
    /// only `&mut self`. So the cursor is `take`n for each step and the successor put back.
    ///
    /// `None` means this source is finished, by either route: the archive ended, or a step
    /// failed and consumed the cursor. Every error path leaves it `None` deliberately, so one
    /// error ends the source rather than leaving it half-usable — which matches the policy
    /// the rest of the pipeline already runs on, that one page which cannot be read ends the
    /// run. `next_entry` therefore returns `None` after an error rather than resuming, and
    /// nothing here needs an `expect()`.
    ///
    /// An earlier version of this comment claimed `None` was reachable only through a panic.
    /// That was false — six ordinary error paths reach it — and worse, two paths used to put
    /// the cursor back and carry on without advancing `next_index`, so the next page arrived
    /// under the index the failed one would have had. Both are now uniform.
    archive: Option<OpenArchive<Process, CursorBeforeHeader>>,
    /// Position in the sequence of *yielded* entries, so the writer's key has no gaps where
    /// the archive held something that was not a page.
    next_index: u32,
    names: Names,
    /// Whether a password was supplied, which is all this reader needs: `unrar` was given it
    /// at open, so an encrypted entry either reads or reports its own failure. Without one, an
    /// encrypted entry is refused by name rather than handed to a decoder that will guess.
    has_password: bool,
}

impl RarSource {
    /// Opens the archive at `path`, reading its archive header only.
    ///
    /// [`Naming::ByPosition`] needs an entry total, and rar is the one format with no entry
    /// table to read it from, so it costs a second open in List mode. That mode walks headers
    /// without decoding: measured at 2 ms against a 399 ms read on a solid 95-entry archive,
    /// under one percent of an end-to-end run. It is taken only when the total is wanted, so
    /// a default run pays nothing.
    ///
    /// # Errors
    ///
    /// [`SourceError::UnsafePath`] when `path` holds an interior NUL, and
    /// [`SourceError::Rar`] when the archive header cannot be read.
    pub fn open(path: &Path, options: &ReadOptions) -> Result<Self, SourceError> {
        // Before the path reaches `unrar`, which panics rather than erroring on an interior
        // NUL: `open_for_processing` is documented `# Panics`, via
        // `WideCString::from_os_str(path).expect("Unexpected nul in path")`. A command-line
        // argument cannot carry one on either release target, but the library API is public,
        // and a panic in the reader thread costs the run its message.
        if path.to_string_lossy().contains('\0') {
            return Err(SourceError::UnsafePath);
        }

        let names = match options.naming {
            Naming::Stored => Names::stored(),
            Naming::ByPosition => Names::by_position(count_entries(path)?),
        };

        // `options.charset` is not consulted, and this is the module the reason belongs in:
        // the stored bytes are gone before Rust sees a name. See the module doc.
        let archive = match &options.password {
            Some(password) => unrar_ng::Archive::with_password(path, password),
            None => unrar_ng::Archive::new(path),
        };

        Ok(Self {
            archive: Some(archive.open_for_processing()?),
            next_index: 0,
            names,
            has_password: options.password.is_some(),
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
            //   5. the encryption flag     — refusable before any data is read
            //   6. the recorded size       — likewise
            //   7. the bytes               — bounded on the way in, then probed
            //   8. the stored name         — refused only once it is a page worth naming
            //
            // 1 before 4 is the point of the flag: `SourceError::Mismatch` ("named as JPEG but
            // its leading bytes are not") would be the wrong diagnosis for a directory. 2 that
            // early because a volume set is refused whatever the entry happens to be — the
            // failure is that the input is one part of a set, not that some page is odd. 7
            // last, and it matters: a traversing name that is also not a JPEG is reported as
            // the mismatch, because a name is only worth refusing on once the thing it names
            // is a page. `zip.rs` orders it the same way.
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

            // 3 before 4, which is the only order in which 3 can fire at all: a name ending in
            // a separator has no extension for `declared_format` to claim, so checking it
            // afterwards would make it unreachable. `zip.rs` checks it first for the same
            // reason.
            if is_directory(&name) {
                match header.skip() {
                    Ok(next) => {
                        self.archive = Some(next);
                        continue;
                    }
                    Err(error) => return Some(Err(error.into())),
                }
            }

            let Some(declared) = probe::declared_format(&name) else {
                match header.skip() {
                    Ok(next) => {
                        self.archive = Some(next);
                        continue;
                    }
                    Err(error) => return Some(Err(error.into())),
                }
            };

            // After the filter, so a non-page entry in an encrypted archive is passed over
            // rather than refused, and before the read, so ciphertext never reaches a page
            // buffer. `unrar` would otherwise report "bad password" for an entry nothing was
            // supplied for.
            if header.entry().is_encrypted() && !self.has_password {
                return Some(Err(SourceError::Encrypted { name }));
            }

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
            let next = match result {
                Ok(next) => next,
                // The sink refused, so the entry holds more than it declared. The archive is
                // not returned on this path and is not wanted: one page that fails ends the
                // run.
                Err(error) if error.code == Code::Aborted && error.when == When::Process => {
                    return Some(Err(SourceError::TooLarge {
                        name,
                        limit: MAX_ENTRY_BYTES,
                    }));
                }
                // The archive is readable and this entry is not — an encrypted entry in a
                // plain-header archive is the case that reaches here — so the entry is named.
                // `SourceError::Rar` is for the archive's own structure and would say
                // "cannot read the archive" about an archive that is fine.
                Err(source) => return Some(Err(SourceError::RarEntry { name, source })),
            };
            let bytes = sink.bytes;

            // The cursor is deliberately not stored back until this entry has passed every
            // check. Every `return Some(Err(..))` below drops `next`, which closes the
            // archive and leaves `self.archive` at `None` — so one error ends the source
            // rather than leaving it resumable at an index it has already used. See the
            // field's doc.

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

            self.archive = Some(next);

            self.next_index += 1;
            return Some(Ok(Entry {
                index,
                name: self.names.of(&name),
                format: declared,
                bytes,
            }));
        }
    }
}

/// How many entries the archive at `path` holds, from a header-only walk.
///
/// List mode does not decode, which is what makes this affordable on a solid archive: a
/// counting pass built on `skip()` would decode every entry to keep the dictionary coherent,
/// and cost a second full read. Measured on a solid 95-entry RAR 5.0 archive at 2 ms against
/// a 399 ms read.
///
/// The count is entries rather than pages, matching every other format: an exact page count
/// would make this pass duplicate the extension filter to move a digit in a book of exactly
/// 100 candidate pages holding one entry that is not a page.
fn count_entries(path: &Path) -> Result<usize, SourceError> {
    let mut entries = 0;
    for header in unrar_ng::Archive::new(path).open_for_listing()? {
        header?;
        entries += 1;
    }
    Ok(entries)
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
