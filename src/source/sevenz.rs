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
//!
//! # What this module bounds, and what it cannot
//!
//! Three allocations in a 7z read are sized by numbers the archive chooses, and only one of
//! them is reachable from here.
//!
//! The LZMA2 dictionary is: the size is in the header the crate has already parsed when
//! `ArchiveReader::new` returns, so [`MAX_DICTIONARY_BYTES`] is applied to it before a single
//! block is decoded. The crate's own guard cannot help — `MAX_MEM_LIMIT_KB` is
//! `usize::MAX / 1024` — and it exposes no knob, so the ceiling lives here.
//!
//! The header's own parse is not, and it is two allocations rather than one. An encoded
//! header is decompressed into a `Vec` bounded by the archive's declared unpack size, and the
//! entry table is then an infallible `vec![ArchiveEntry::default(); num_files]`. The encoded
//! header is *itself* decoded through a coder chain, and `read_encoded_header`
//! (`reader.rs:466`) hands that chain the same `MAX_MEM_LIMIT_KB` that cannot fire — so a
//! header folder declaring a 4 GiB dictionary allocates it too. All three happen inside
//! `Archive::read`, before any code here runs, with no knob to set. A padded, highly
//! compressible header is therefore small on disk and large in memory. Recorded rather than
//! forked around: `AGENTS.md` records what maintaining a fork of a reader cost the Go
//! implementation, and the fix belongs upstream. The residual is a hostile archive making
//! this process allocate at parse time; an ordinary one is unaffected, and the ceiling above
//! still bounds every block the walk decodes.

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

/// The largest LZMA or LZMA2 dictionary this build lets an archive ask for.
///
/// The size is the *archive's* choice, and the format lets it be 4 GiB: an LZMA2 property
/// byte of 40 means `0xFFFF_FFFF`. `sevenz-rust2` has a guard for this and it cannot fire —
/// `MAX_MEM_LIMIT_KB` is `usize::MAX / 1024` — and it exposes no knob to lower, so the ceiling
/// is applied here from the header the crate already parsed, before a single block is decoded.
///
/// 256 MiB, and the figure is bounded from both sides rather than picked. Below it: a typical
/// archive measured at 32 MiB, and an archive written with `-m0=LZMA2:d128m` at 96 MiB, which
/// is what 7-Zip clamps that request to on a 72 MB input; the largest dictionary any `-mx`
/// preset selects is 64 MiB. Above it: the 4 GiB the format allows, which is the allocation
/// this exists to refuse. An archive written with a deliberately larger dictionary is refused
/// by name rather than by an allocator.
pub const MAX_DICTIONARY_BYTES: u64 = 256 << 20;

/// The largest dictionary any block declares, when that exceeds [`MAX_DICTIONARY_BYTES`].
///
/// Read off the coder properties the header already carries, decoded the way the crate's own
/// `get_lzma2_dic_size` and `get_lzma_dic_size` decode them. A property field this does not
/// recognise is left to the crate, which is the only party that can say what it means.
fn oversized_dictionary(archive: &sevenz_rust2::Archive) -> Option<u64> {
    archive
        .blocks
        .iter()
        .flat_map(|block| &block.coders)
        .filter_map(|coder| dictionary_bytes(coder.encoder_method_id(), coder.properties()))
        .max()
        .filter(|&declared| declared > MAX_DICTIONARY_BYTES)
}

/// The dictionary one coder declares, or `None` for a method whose properties do not name one.
///
/// Split out from [`oversized_dictionary`] because this is the part that reads bytes an
/// attacker chose, and a test can drive it with those bytes directly. Every arm is total: a
/// short, absent or unrecognised property field gives `None` and is left to the crate, which
/// is the only party that can say what it means.
fn dictionary_bytes(method: &[u8], properties: &[u8]) -> Option<u64> {
    match method {
        // One byte: `(2 | (bits & 1)) << (bits / 2 + 11)`, with 40 meaning the 4 GiB maximum
        // and anything above it rejected by the crate before it is used.
        sevenz_rust2::EncoderMethod::ID_LZMA2 => {
            let bits = u32::from(*properties.first()?);
            match bits {
                41.. => None,
                40 => Some(u64::from(u32::MAX)),
                _ => Some(u64::from((2 | (bits & 1)) << (bits / 2 + 11))),
            }
        }
        // Five bytes: one property byte, then the size as a little-endian `u32`.
        sevenz_rust2::EncoderMethod::ID_LZMA => {
            let field: [u8; 4] = properties.get(1..5)?.try_into().ok()?;
            Some(u64::from(u32::from_le_bytes(field)))
        }
        _ => None,
    }
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
    /// headers are encrypted and one written with a codec this build does not carry, and
    /// [`SourceError::Dictionary`] when a block declares more working memory than
    /// [`MAX_DICTIONARY_BYTES`].
    pub fn new(file: std::fs::File, naming: Naming) -> Result<Self, SourceError> {
        let mut reader = ArchiveReader::new(file, Password::empty())?;
        // Not a trade. `ArchiveReader::new` defaults to `available_parallelism()`, which
        // selects the multi-threaded LZMA2 reader and gives every worker its own dictionary:
        // measured on one 512 MiB solid block, 225 ms and 35.6 MB at one thread against
        // 284 ms and 682.3 MB at ten. Single-threaded is both faster and nineteen times
        // leaner, and it leaves the cores to the pipeline's own workers.
        reader.set_thread_count(1);

        if let Some(declared) = oversized_dictionary(reader.archive()) {
            return Err(SourceError::Dictionary {
                declared,
                limit: MAX_DICTIONARY_BYTES,
            });
        }

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

/// Walks every entry, offering each page across `sender` until the walk ends.
///
/// Errors travel in band rather than out of the thread's return value, so the pipeline meets
/// them in the same order it meets pages.
///
/// # How the walk is stopped, and why not with `false`
///
/// The callback's `Ok(false)` looks like the way to stop, and it is not one.
/// `BlockDecoder::for_each_entries` honours it, but `ArchiveReader::for_each_entries`
/// discards the `bool` its block decoder returns (`reader.rs:1653`,
/// `forder_dec.for_each_entries(&mut each)?;`) and starts the next block. So `false` ends the
/// current block and nothing more — and a multi-block archive is the ordinary case, since
/// 7-Zip splits by type and size and `-ms=off` gives one block per entry.
///
/// Measured: a two-block archive whose first block holds a non-JPEG named `a_bad.jpg` and
/// whose second holds a gibibyte of zeros packed to 230 KB refused in 0.55 s, against 0.00 s
/// for the same first block alone. The refusal was decided from two bytes; the rest was the
/// second block being decoded into a sink after the callback had said stop.
///
/// `Err` is the seam that works: both loops propagate it with `?`. So the callback returns a
/// sentinel error and `stopped` records that the sentinel was ours, so the walk's own error
/// is not reported twice or mistaken for the archive's.
fn decode(
    mut reader: ArchiveReader<std::fs::File>,
    mut names: Names,
    sender: &Sender<Result<Entry, SourceError>>,
) {
    let mut next_index = 0;
    let mut stopped = false;

    let walked = reader.for_each_entries(|entry, stream| {
        let mut drain = Drain::new(stream);
        let name = entry.name.clone();

        // The one exception below goes its own way: `finish` moves the drain, so a skip whose
        // drain fails cannot reach this. Every other terminal path does, and each one
        // abandons the rest of the entry — a path that ends the run has no next entry to keep
        // the block coherent for — and then ends the whole walk rather than this block.
        macro_rules! stop {
            ($result:expr) => {{
                drain.abandon();
                offer(sender, $result);
                stopped = true;
                return Err(stop());
            }};
        }

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
        //
        // The first three share one exit because a skip is the one path that continues, so it
        // is the one path that must drain — and a drain that fails is reported rather than
        // swallowed. `.is_ok()` would turn a corrupt block into a clean end of archive: fewer
        // pages, exit code zero.
        let claimed = if entry.is_directory() || is_directory(&name) {
            None
        } else {
            probe::declared_format(&name)
        };
        let Some(declared) = claimed else {
            return match drain.finish() {
                Ok(()) => Ok(true),
                Err(source) => {
                    offer(sender, Err(SourceError::Entry { name, source }));
                    stopped = true;
                    Err(stop())
                }
            };
        };

        // The header records the size away from the entry's data, so an entry claiming more
        // than the limit is refused before any of it is decoded — which is what `abandon`
        // inside `stop!` buys: without it `Drain::drop` would decompress the whole entry on
        // the way out, and a 229 KB archive declaring a gibibyte cost 0.55 s of CPU.
        if entry.size > MAX_ENTRY_BYTES {
            stop!(Err(too_large(name)));
        }

        let index = next_index;

        let mut head = [0; MAGIC_MAX];
        let head = match fill(drain.stream(), &mut head) {
            Ok(read) => &head[..read],
            Err(source) => stop!(Err(SourceError::Entry { name, source })),
        };

        // The extension said this was a page. If the bytes disagree the archive is
        // inconsistent, which is an error rather than a skip: dropping the page would
        // shorten the book.
        match probe::probe(head) {
            Some(format) if format == declared => {}
            _ => stop!(Err(SourceError::Mismatch {
                name,
                declared: declared.name(),
            })),
        }

        let bytes = match read_entry(&mut drain, entry.size, head) {
            Ok(bytes) if bytes.len() as u64 > MAX_ENTRY_BYTES => stop!(Err(too_large(name))),
            Ok(bytes) => bytes,
            Err(source) => stop!(Err(SourceError::Entry { name, source })),
        };

        // The stored name goes into the *output* archive, so a traversing or absolute name
        // would be carried to whatever extracts it. Refused rather than sanitised.
        if let Some(reason) = unsafe_name(&name) {
            stop!(Err(SourceError::UnsafeName { name, reason }));
        }

        next_index += 1;
        if offer(
            sender,
            Ok(Entry {
                index,
                name: names.of(&name, declared),
                format: declared,
                bytes,
            }),
        ) {
            Ok(true)
        } else {
            // The receiver is gone — the pipeline stopped, usually because an earlier page
            // failed — so there is nobody left to read the rest of the archive for. Nothing
            // to offer: the pipeline already has its own error.
            //
            // `read_entry` has consumed this entry to its end on every path that reaches
            // here, so the abandon costs nothing today. It is here because that is an
            // invariant two functions apart with nothing else recording it, and relying on
            // one of those is what the guard exists to stop.
            drain.abandon();
            stopped = true;
            Err(stop())
        }
    });

    // The archive's own structure, reached after some entries may already have been offered.
    // Sent like any other error so the pipeline meets it in read order — unless the walk
    // ended because this module ended it, in which case the real error is already in flight.
    match walked {
        Ok(()) => {}
        Err(_) if stopped => {}
        Err(error) => {
            offer(sender, Err(error.into()));
        }
    }
}

/// The sentinel that ends the walk, because `Ok(false)` only ends one block.
///
/// Never reaches a user: `decode` recognises it by the `stopped` flag it is always set with,
/// and the error the caller should see was offered across the channel first.
fn stop() -> sevenz_rust2::Error {
    sevenz_rust2::Error::Other(std::borrow::Cow::Borrowed("the reader ended the walk"))
}

fn too_large(name: String) -> SourceError {
    SourceError::TooLarge {
        name,
        limit: MAX_ENTRY_BYTES,
    }
}

/// Hands `result` to the pipeline, reporting whether the receiver is still there.
fn offer(sender: &Sender<Result<Entry, SourceError>>, result: Result<Entry, SourceError>) -> bool {
    sender.send(result).is_ok()
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

/// Keeps a solid block coherent across an entry the callback did not read to its end.
///
/// A guard rather than a rule, because forgetting is not an option the type offers: a skip, an
/// early return and a `?` all drain by default. Two named exits depart from that default and
/// both are deliberate:
///
/// - [`Drain::finish`] drains and *reports*, so a corrupt block on the one path that
///   continues is an error rather than a quiet end of archive.
/// - [`Drain::abandon`] does not drain at all, for a path that ends the whole walk. There is
///   no next entry to keep coherent, and draining would decode an entry that was refused
///   precisely so it would not be decoded — measured at 0.55 s of CPU for a 229 KB archive
///   declaring a gibibyte.
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

    /// Leaves the rest of the entry undecoded, for a path that ends the walk.
    fn abandon(&mut self) {
        self.drained = true;
    }
}

impl Drop for Drain<'_> {
    fn drop(&mut self) {
        if !self.drained {
            let _ = io::copy(&mut self.stream, &mut io::sink());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Drain, MAX_DICTIONARY_BYTES, dictionary_bytes};
    use std::io::{self, Read};

    /// A reader that reports how much of it was consumed, and can be made to fail.
    struct Counting {
        remaining: usize,
        read: usize,
        fails_after: usize,
    }

    impl Read for Counting {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.read >= self.fails_after {
                return Err(io::Error::other("the block is corrupt"));
            }
            let take = buffer.len().min(self.remaining).min(64);
            self.remaining -= take;
            self.read += take;
            buffer[..take].fill(b'x');
            Ok(take)
        }
    }

    fn reader(bytes: usize) -> Counting {
        Counting {
            remaining: bytes,
            read: 0,
            fails_after: usize::MAX,
        }
    }

    /// The default exit: whatever the callback left is consumed, so the next entry in a solid
    /// block starts where the decoder thinks it does.
    #[test]
    fn a_dropped_guard_consumes_the_rest_of_the_entry() {
        let mut stream = reader(1000);
        {
            let mut drain = Drain::new(&mut stream);
            let mut head = [0; 4];
            drain.stream().read_exact(&mut head).expect("reads a head");
        }
        assert_eq!(stream.read, 1000, "the guard left bytes in the block");
    }

    /// The named exit that continues: it drains, and it *reports* a drain that failed rather
    /// than turning a corrupt block into a quiet end of archive.
    #[test]
    fn finish_drains_and_reports_a_failure() {
        let mut stream = reader(1000);
        Drain::new(&mut stream).finish().expect("drains");
        assert_eq!(stream.read, 1000);

        let mut broken = Counting {
            remaining: 1000,
            read: 0,
            fails_after: 128,
        };
        Drain::new(&mut broken)
            .finish()
            .expect_err("a drain failure must be reported, not swallowed");
    }

    /// The named exit that ends the walk: nothing is consumed, because there is no next entry
    /// to keep coherent and decoding an entry that was refused is the cost the refusal exists
    /// to avoid.
    #[test]
    fn abandon_leaves_the_entry_undecoded() {
        let mut stream = reader(1000);
        {
            let mut drain = Drain::new(&mut stream);
            drain.abandon();
        }
        assert_eq!(
            stream.read, 0,
            "an abandoned entry was decoded on the way out"
        );
    }

    /// The decode itself, against the bytes an archive would carry, and checked against
    /// `sevenz-rust2`'s own `get_lzma2_dic_size` and `get_lzma_dic_size` rather than against a
    /// restatement of them. The property byte 30 is 128 MiB — the largest a plausible archive
    /// declares — 32 is exactly the ceiling, and 40 is the 4 GiB maximum the format allows,
    /// which is the allocation the ceiling exists to refuse.
    #[test]
    fn a_declared_dictionary_is_decoded_from_the_bytes_the_coder_carries() {
        let lzma2 = sevenz_rust2::EncoderMethod::ID_LZMA2;
        let lzma = sevenz_rust2::EncoderMethod::ID_LZMA;

        assert_eq!(dictionary_bytes(lzma2, &[30]), Some(128 << 20));
        assert_eq!(dictionary_bytes(lzma2, &[32]), Some(256 << 20));
        assert_eq!(dictionary_bytes(lzma2, &[40]), Some(u64::from(u32::MAX)));
        // Above 40 the crate refuses the byte itself, so there is nothing here to bound.
        assert_eq!(dictionary_bytes(lzma2, &[41]), None);
        assert_eq!(dictionary_bytes(lzma2, &[]), None);

        // One property byte, then the size little-endian. Unlike LZMA2 there is no capped
        // encoding here — the field is the size — so the 4 GiB maximum is a plain `u32::MAX`.
        assert_eq!(
            dictionary_bytes(lzma, &[0x5D, 0x00, 0x00, 0x00, 0x10]),
            Some(256 << 20)
        );
        assert_eq!(
            dictionary_bytes(lzma, &[0x5D, 0xFF, 0xFF, 0xFF, 0xFF]),
            Some(u64::from(u32::MAX))
        );
        assert_eq!(dictionary_bytes(lzma, &[0x5D, 0x00, 0x00]), None);

        // A filter names no dictionary, whatever its properties hold.
        assert_eq!(
            dictionary_bytes(sevenz_rust2::EncoderMethod::ID_DELTA, &[4]),
            None
        );
        assert_eq!(dictionary_bytes(&[0x03, 0x03, 0x01, 0x03], &[]), None);
    }

    /// The ceiling, against the same decode: what a real archive declares passes and what the
    /// format's maximum declares does not.
    #[test]
    fn the_dictionary_ceiling_admits_the_plausible_and_refuses_the_possible() {
        let lzma2 = sevenz_rust2::EncoderMethod::ID_LZMA2;
        let declared = |bits: u8| dictionary_bytes(lzma2, &[bits]).expect("a dictionary");

        assert!(declared(30) <= MAX_DICTIONARY_BYTES, "128 MiB must pass");
        assert!(
            declared(32) <= MAX_DICTIONARY_BYTES,
            "256 MiB is the ceiling"
        );
        assert!(
            declared(34) > MAX_DICTIONARY_BYTES,
            "512 MiB must be refused"
        );
        assert!(declared(40) > MAX_DICTIONARY_BYTES, "4 GiB must be refused");
    }
}
