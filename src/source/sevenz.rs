//! Ordered, single-pass 7z reading, through a decoder that pushes rather than being asked.
//!
//! `sevenz-rust2` offers one streaming path and it is push-based:
//!
//! ```text
//! for_each_entries<F: FnMut(&ArchiveEntry, &mut dyn Read) -> Result<bool, Error>>
//! ```
//!
//! [`Entries`](super::Entries) is pull-based, and the crate has no pull equivalent: `mod
//! decoder` is `pub(crate)`, `BlockDecoder::for_each_entries` takes `self` by value, and
//! `read_file(name)` is documented by its own author as decoding everything before the
//! requested file, which is quadratic over a solid block. So the callback runs on a thread of
//! this module's making and each entry crosses a rendezvous channel.
//!
//! Assembling the decode stack out of the crate's public internals would remove the thread.
//! It is refused on the principle `AGENTS.md` names: it reimplements `decoder.rs::add_decoder`
//! outside the crate, which is the forked-zip-reader mistake in a new costume, and it would
//! rot silently the first time those internals moved under a version bump.
//!
//! # The one entry this costs, and where it is accounted for
//!
//! A push-shaped source has already read an entry by the time it offers it, so while blocked
//! on `send` it holds one entry the pipeline's credit system has not accounted for. The window
//! is `W + 1` for this source; see [`crate::pipeline`], where the bound is argued.
//!
//! # A skipped entry must be drained, and the failure is silent
//!
//! `BlockDecoder::for_each_entries` does not drain an entry the callback leaves partially
//! read. Measured on a six-entry solid block with distinct markers, skipping every third entry
//! without draining:
//!
//! ```text
//! page1.jpg MARK01--   page2.jpg SKIPPED   page3.jpg MARK02--   [iteration stops, 3 of 6]
//! ```
//!
//! `page3.jpg` does not get zeros — it gets page 2's bytes, and iteration ends three entries
//! early. The wrong page is a structurally valid JPEG, so nothing downstream can notice: not
//! the decoder, not the encoder, and not the CRC, which only fires on a fully consumed entry.
//! [`Drain`] closes it structurally rather than by remembering to.
//!
//! # An entry with no stream arrives after every entry that has one
//!
//! `ArchiveReader::for_each_entries` walks the compression blocks first and then every entry
//! the header gave no stream — a directory, or a zero-byte file. Pages always have a stream,
//! so they arrive in stored order; the reordering is confined to entries that are not pages.
//! The one case it shows in is a zero-byte `.jpg`, whose extension claims a page its bytes
//! cannot be, and which is therefore refused later in the run than its position suggests.
//! Recorded rather than worked around: driving `BlockDecoder` per block to fix the order would
//! stop visiting those entries at all, which turns a late refusal into a page silently missing
//! from the book.

use std::io::{self, Read};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, bounded};
use sevenz_rust2::{ArchiveReader, Password};

use super::probe::{self, MAGIC_MAX, Names, Naming};
use super::{Entry, HINT_CEILING, MAX_ENTRY_BYTES, SourceError, fill, is_directory, unsafe_name};

/// A 7z archive being decoded on its own thread, one entry at a time.
pub struct SevenZSource {
    /// Each entry as the decoder produces it, or the error that ended the walk. Capacity
    /// zero, so the decoder is never further ahead than the one entry it is offering.
    ///
    /// `None` once the channel has closed and the thread has been joined, which is what makes
    /// [`SevenZSource::next_entry`] return `None` for good rather than joining twice.
    entries: Option<Receiver<Result<Entry, SourceError>>>,
    /// Kept only so a panic in the decoder becomes an error rather than a lost run: the
    /// pipeline's `catch_unwind` sits on the *outer* reader thread and would not see this one.
    decoder: Option<JoinHandle<()>>,
}

impl SevenZSource {
    /// Opens `file` as a 7z archive, reading its header.
    ///
    /// The header is parsed here rather than on the decoder thread, so a malformed archive
    /// fails before the output file is created — the same point zip fails at.
    ///
    /// # Errors
    ///
    /// [`SourceError::SevenZ`] when the header cannot be read, which covers an archive whose
    /// headers are encrypted and one written with a codec this build does not carry.
    pub fn new(file: std::fs::File, naming: Naming) -> Result<Self, SourceError> {
        let mut reader = ArchiveReader::new(file, Password::empty())?;
        // Not a trade. `ArchiveReader::new` defaults to `available_parallelism()`, which
        // selects the multi-threaded LZMA2 reader and gives every worker its own dictionary:
        // measured on one 512 MiB solid block, 225 ms and 35.6 MB at one thread against
        // 284 ms and 682.3 MB at ten. Single-threaded is both faster and nineteen times
        // leaner, and it leaves the cores to the pipeline's own workers.
        reader.set_thread_count(1);

        let names = match naming {
            Naming::Stored => Names::stored(),
            // The header is parsed by now, so the entry total costs nothing here.
            Naming::ByPosition => Names::by_position(reader.archive().files.len()),
        };

        let (sender, entries) = bounded(0);
        let decoder = std::thread::spawn(move || decode(reader, names, &sender));

        Ok(Self {
            entries: Some(entries),
            decoder: Some(decoder),
        })
    }

    /// The next page, or `None` at the end of the archive.
    pub fn next_entry(&mut self) -> Option<Result<Entry, SourceError>> {
        if let Ok(entry) = self.entries.as_ref()?.recv() {
            return Some(entry);
        }
        // The decoder dropped its sender, so it has either finished or panicked. Joining
        // here is what turns the second into an error: the pipeline's own `catch_unwind`
        // sits on the outer reader thread and would not see this one.
        self.entries = None;
        match self.decoder.take()?.join() {
            Ok(()) => None,
            Err(_) => Some(Err(SourceError::SevenZPanicked)),
        }
    }
}

impl Drop for SevenZSource {
    /// Joins the decoder, so no thread outlives the source that started it.
    ///
    /// The receiver goes first and the order is load-bearing: the decoder may be blocked
    /// offering an entry, and joining while it is still connected would wait for a consumer
    /// that is in the middle of being dropped. Once disconnected the send fails, the callback
    /// stops, and the join costs at most the tail of one entry's decode.
    fn drop(&mut self) {
        self.entries = None;
        if let Some(decoder) = self.decoder.take() {
            let _ = decoder.join();
        }
    }
}

/// Walks every entry, offering each page across `sender` until the receiver goes away.
///
/// Errors travel in band rather than out of the thread's return value, so the pipeline meets
/// them in the same order it meets pages.
fn decode(
    mut reader: ArchiveReader<std::fs::File>,
    mut names: Names,
    sender: &Sender<Result<Entry, SourceError>>,
) {
    let mut next_index = 0;

    let walked = reader.for_each_entries(|entry, stream| {
        let mut drain = Drain::new(stream);
        let name = entry.name.clone();

        // The order of these checks decides which error a multiply-wrong entry gets, and it
        // is `zip.rs`'s order for `zip.rs`'s reasons:
        //
        //   1. the directory flag      — not a page at all, so no page diagnosis applies
        //   2. the trailing separator  — a directory that failed to set its flag
        //   3. the extension filter    — not a page, and cheap to decide from the name
        //   4. the recorded size       — refusable before any data is read
        //   5. the leading bytes       — read on their own, so a mismatch costs two bytes
        //   6. the rest of the entry   — bounded on the way in
        //   7. the stored name         — refused only once it is a page worth naming
        if entry.is_directory() || is_directory(&name) {
            return Ok(drain.finish().is_ok());
        }
        let Some(declared) = probe::declared_format(&name) else {
            return Ok(drain.finish().is_ok());
        };

        // The header records the size away from the entry's data, so an entry claiming more
        // than the limit costs nothing to refuse and is not read at all.
        if entry.size > MAX_ENTRY_BYTES {
            return Ok(offer(sender, Err(too_large(name))));
        }

        let index = next_index;

        let mut head = [0; MAGIC_MAX];
        let head = match fill(drain.stream(), &mut head) {
            Ok(read) => &head[..read],
            Err(source) => return Ok(offer(sender, Err(SourceError::Entry { name, source }))),
        };

        // The extension said this was a page. If the bytes disagree the archive is
        // inconsistent, which is an error rather than a skip: dropping the page would
        // shorten the book.
        match probe::probe(head) {
            Some(format) if format == declared => {}
            _ => {
                return Ok(offer(
                    sender,
                    Err(SourceError::Mismatch {
                        name,
                        declared: declared.name(),
                    }),
                ));
            }
        }

        let bytes = match read_entry(&mut drain, entry.size, head) {
            Ok(bytes) if bytes.len() as u64 > MAX_ENTRY_BYTES => {
                return Ok(offer(sender, Err(too_large(name))));
            }
            Ok(bytes) => bytes,
            Err(source) => return Ok(offer(sender, Err(SourceError::Entry { name, source }))),
        };

        // The stored name goes into the *output* archive, so a traversing or absolute name
        // would be carried to whatever extracts it. Refused rather than sanitised.
        if let Some(reason) = unsafe_name(&name) {
            return Ok(offer(sender, Err(SourceError::UnsafeName { name, reason })));
        }

        next_index += 1;
        Ok(offer(
            sender,
            Ok(Entry {
                index,
                name: names.of(&name, declared),
                format: declared,
                bytes,
            }),
        ))
    });

    // The archive's own structure, reached after some entries may already have been offered.
    // Sent like any other error so the pipeline meets it in read order.
    if let Err(error) = walked {
        offer(sender, Err(error.into()));
    }
}

fn too_large(name: String) -> SourceError {
    SourceError::TooLarge {
        name,
        limit: MAX_ENTRY_BYTES,
    }
}

/// Hands `result` to the pipeline, reporting whether the walk should continue.
///
/// A failed send means the receiver is gone — the pipeline stopped, usually because an
/// earlier page failed — and there is nobody left to read the rest of the archive for. An
/// error stops the walk for the reason every other reader stops on one: a run that cannot
/// produce one page does not produce a book.
fn offer(sender: &Sender<Result<Entry, SourceError>>, result: Result<Entry, SourceError>) -> bool {
    let carry_on = result.is_ok();
    sender.send(result).is_ok() && carry_on
}

/// Reads the rest of one entry after `head`, bounded independently of its declared size.
///
/// The bound on the read stays alongside the check on the recorded size, exactly as in
/// `zip.rs`: a recorded size that disagrees with what the entry holds is the malformed case,
/// and a check that trusts the number it is validating is not a check. One byte past the
/// limit, so an entry exactly at it is accepted and anything larger is detectable without
/// reading the rest.
fn read_entry(drain: &mut Drain<'_>, recorded: u64, head: &[u8]) -> io::Result<Vec<u8>> {
    // Capped for the reason `zip.rs` caps it: a hundred-byte entry could record 64 MiB and
    // get 64 MiB reserved, and up to `2 * jobs` of those buffers are alive at once.
    let hint = usize::try_from(recorded.min(HINT_CEILING)).unwrap_or(0);
    let mut bytes = Vec::with_capacity(hint.saturating_add(head.len()));
    bytes.extend_from_slice(head);

    let remaining = MAX_ENTRY_BYTES
        .saturating_sub(bytes.len() as u64)
        .saturating_add(1);
    drain.stream().take(remaining).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Consumes whatever is left of an entry, on every path out of the callback.
///
/// A guard rather than a rule, because forgetting is not an option the type offers: a skip, an
/// early return and a `?` all drain. Only a *skip that continues* strictly needs it — every
/// error path ends the run, so the block's state afterwards is irrelevant — but making the
/// correct thing automatic is what stops the next reader of this file getting it wrong.
struct Drain<'a> {
    stream: &'a mut dyn Read,
    drained: bool,
}

impl<'a> Drain<'a> {
    fn new(stream: &'a mut dyn Read) -> Self {
        Self {
            stream,
            drained: false,
        }
    }

    fn stream(&mut self) -> &mut dyn Read {
        self.stream
    }

    /// Drains explicitly, so a drain failure is reported rather than swallowed in `Drop`.
    fn finish(mut self) -> io::Result<()> {
        self.drained = true;
        io::copy(&mut self.stream, &mut io::sink()).map(|_| ())
    }
}

impl Drop for Drain<'_> {
    fn drop(&mut self) {
        if !self.drained {
            let _ = io::copy(&mut self.stream, &mut io::sink());
        }
    }
}
