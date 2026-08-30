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
//!
//! # Why every stored name is read when the archive is opened
//!
//! A name's *bytes* are reachable only from a located entry — `ZipArchive` has no raw-name
//! accessor, every `pub fn` on it was enumerated, and `metadata()` hands back a type with no
//! way into its map. And the encoding those bytes are in has to be chosen for the whole
//! archive rather than per entry, so the whole name set is needed before the first page.
//!
//! So the names are surveyed at open, through `by_index_raw`, and decoded once. Measured with
//! a counting reader: `find_data_start` seeks to the local header, parses the fixed block, and
//! computes the data offset arithmetically — thirty bytes and two seeks per entry, no entry
//! data — and memoises the offset in a `OnceLock` the archive owns, so the `by_index` a page
//! costs later re-reads nothing. The added cost against the previous build is therefore thirty
//! bytes for each entry the extension filter rejects, and nothing for the pages.
//!
//! `by_index_raw` rather than `by_index`, and the difference is the point: it builds no
//! decompressor and no crypto reader, so an entry whose codec this build lacks or whose data
//! is encrypted still yields its name. Locating with `by_index` instead would make a run fail
//! on a `ComicInfo.xml` the filter was about to drop. It is never used to read *data* — for
//! that it hands back ciphertext as plaintext with no error, measured — and `read_one` does
//! not call it.

use std::io::{Read, Seek, SeekFrom};

use zip::{AesMode, HasZipMetadata, ZipArchive, ZipReadOptions};

use super::charset::Stated;
use super::probe::{self, MAGIC_MAX, Names, Naming};
use super::{
    Entry, HINT_CEILING, MAX_ENTRY_BYTES, ReadOptions, SourceError, fill, is_directory, unsafe_name,
};

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
    /// Every entry's name, decoded once when the archive was opened, in table order. See the
    /// module doc for why it cannot be derived one entry at a time.
    decoded: Vec<String>,
    /// Each entry's encryption, learned in the same survey: the flag and the AE-x extra field
    /// are both reachable only from a located entry.
    encryption: Vec<Encryption>,
    /// The password an encrypted entry is read with, or `None` to refuse one.
    password: Option<Vec<u8>>,
}

impl<R: Read + Seek> ZipSource<R> {
    /// Reads the archive's entry table and every stored name.
    ///
    /// # Errors
    ///
    /// [`SourceError::Archive`] when the entry table cannot be read. It is read here, before
    /// any entry, so a truncated or malformed one fails the run before a page is processed.
    /// [`SourceError::RepeatedName`] when the table kept fewer entries than the archive
    /// records, which is what two entries stored under one name look like from here.
    /// [`SourceError::Charset`] when no listed encoding decodes every name the archive left
    /// undeclared.
    pub fn new(mut reader: R, options: &ReadOptions) -> Result<Self, SourceError> {
        let recorded = recorded_entry_count(&mut reader).map_err(::zip::result::ZipError::Io)?;
        let mut archive = ZipArchive::new(reader)?;
        if let Some(recorded) = recorded {
            let kept = archive.len() as u64;
            if recorded > kept {
                return Err(SourceError::RepeatedName { recorded, kept });
            }
        }
        let names = match options.naming {
            Naming::Stored => Names::stored(),
            // The entry table is read by now, so the entry total costs nothing here.
            Naming::ByPosition => Names::by_position(archive.len()),
        };
        let (stated, encryption) = survey(&mut archive);
        let decoded = options.charset.decode_all(&stated)?;
        Ok(Self {
            archive,
            next_position: 0,
            next_index: 0,
            names,
            decoded,
            encryption,
            password: options.password.as_ref().map(|pw| pw.as_bytes().to_vec()),
        })
    }

    /// The next page, or `None` at the end of the archive.
    ///
    /// Entries that are not pages are passed over without being read: a directory, and any
    /// entry whose extension no candidate claims, cost nothing beyond the name the survey
    /// already decoded.
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

    /// Why the entry at `position` must not be read, or `None` if it may be.
    ///
    /// Both refusals come before the entry is located, because locating an encrypted entry is
    /// what produces the dependency's own diagnosis — `InvalidPassword` for an AES entry with
    /// no password, which is the wrong answer twice over: no password would have helped, and
    /// the form is what the user needs told.
    fn refuse_encryption(&self, position: usize, name: &str) -> Option<SourceError> {
        let encryption = self.encryption[position];
        if let Some(form) = encryption.unsupported_form() {
            return Some(SourceError::EncryptionUnsupported {
                name: name.to_owned(),
                form,
            });
        }
        if encryption.encrypted && self.password.is_none() {
            return Some(SourceError::Encrypted {
                name: name.to_owned(),
            });
        }
        None
    }

    /// One pass over the entry at `position` in the table.
    fn read_one(&mut self, position: usize) -> Yielded {
        // The decoded name, not `name_for_index`'s: that one is the archive's own guess, and
        // for an archive that declares no encoding the guess is what this Change exists to
        // replace. Borrowed rather than cloned — `decoded`, `archive`, `names` and `password`
        // are separate fields, so the reads below coexist with locating the entry.
        let Some(name) = self.decoded.get(position) else {
            // `position` came from the archive's own entry count, so this is unreachable.
            // Reported rather than asserted: the reader runs on its own thread, where a
            // panic costs the run its message and buys nothing.
            return Yielded::Failed(SourceError::Archive(::zip::result::ZipError::FileNotFound));
        };
        let name = name.as_str();
        if is_directory(name) {
            return Yielded::Skipped;
        }
        // The extension filter, on the decoded name and before the entry is read. Filtering
        // the archive's guess instead is not merely cosmetic: a GB18030 trail byte may be
        // `6A`, so a legacy name can gain or lose a `.jpg` in the guess.
        let Some(declared) = probe::declared_format(name) else {
            return Yielded::Skipped;
        };

        if let Some(error) = self.refuse_encryption(position, name) {
            return Yielded::Failed(error);
        }
        let encryption = self.encryption[position];
        // A wrong ZipCrypto password is accepted one time in 256, and then the page is
        // garbage rather than the password refused. Recorded here so the two failures that
        // shape takes can say so.
        let false_accept = encryption.encrypted && self.password.is_some();

        let index = self.next_index;
        let options = ZipReadOptions::new().password(self.password.as_deref());
        let mut file = match self.archive.by_index_with_options(position, options) {
            Ok(file) => file,
            // Named, which the sequential reader could not do: it met a malformed entry
            // before its name, where the entry table carries the name of every entry
            // whether or not the entry itself can be read.
            Err(error) => {
                return Yielded::Failed(SourceError::Entry {
                    name: name.to_owned(),
                    source: error.into(),
                });
            }
        };

        // The recorded size is still the archive's claim, but the entry table records it
        // away from the entry's data, so it is known before that data is reached: an entry
        // claiming more than the limit costs nothing to refuse and is not read at all.
        if file.size() > MAX_ENTRY_BYTES {
            return Yielded::Failed(SourceError::TooLarge {
                name: name.to_owned(),
                limit: MAX_ENTRY_BYTES,
            });
        }

        let mut head = [0; MAGIC_MAX];
        let head = match fill(&mut file, &mut head) {
            Ok(read) => &head[..read],
            Err(source) => {
                return Yielded::Failed(SourceError::Entry {
                    name: name.to_owned(),
                    source,
                });
            }
        };

        // The extension said this was a page. If the bytes disagree the archive is
        // inconsistent, which Go also treated as an error rather than a skip.
        match probe::probe(head) {
            Some(format) if format == declared => {}
            _ if false_accept => {
                return Yielded::Failed(SourceError::BadPassword {
                    name: name.to_owned(),
                    reason: format!("the entry does not hold {}", declared.name()),
                });
            }
            _ => {
                return Yielded::Failed(SourceError::Mismatch {
                    name: name.to_owned(),
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
            return Yielded::Failed(if false_accept {
                SourceError::BadPassword {
                    name: name.to_owned(),
                    reason: source.to_string(),
                }
            } else {
                SourceError::Entry {
                    name: name.to_owned(),
                    source,
                }
            });
        }
        if bytes.len() as u64 > MAX_ENTRY_BYTES {
            return Yielded::Failed(SourceError::TooLarge {
                name: name.to_owned(),
                limit: MAX_ENTRY_BYTES,
            });
        }

        // The decoded name goes into the *output* archive, so a traversing or absolute name
        // would be carried to whatever extracts it. `zip`'s own documentation warns that
        // `ZipFile::name` may be absolute or escape via `..`. Rejected rather than
        // sanitised: silently rewriting a name would produce an archive whose entries do not
        // match the input's, which is worse than refusing.
        //
        // After decoding, not before, and that is the load-bearing half: decoding decides
        // which bytes are separators. Shift_JIS consumes `5C` as the trail byte of `表`, so
        // the correct decode has no separator where the archive's guess had one — and a
        // *wrongly* chosen encoding can produce one where the bytes had none, which is
        // exactly what this check is here to refuse.
        if let Some(reason) = unsafe_name(name) {
            return Yielded::Failed(SourceError::UnsafeName {
                name: name.to_owned(),
                reason,
            });
        }

        Yielded::Entry(Entry {
            index,
            name: self.names.of(name, declared),
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

/// What the survey learned about one entry beside its name.
#[derive(Clone, Copy)]
struct Encryption {
    /// General-purpose bit 0. Set means the data is ciphertext.
    encrypted: bool,
    /// The strength an AE-x extra field declared, which is the form this build does not
    /// carry. `None` alongside `encrypted` means `ZipCrypto`, which it does.
    aes: Option<AesMode>,
}

impl Encryption {
    /// The encryption form this build cannot decrypt, if this entry uses one.
    ///
    /// Established from the extra field rather than from the compression method, because
    /// `AexEncryption::parse` rewrites `compression_method` to the *underlying* method — so an
    /// AES entry opens like any other and is indistinguishable from a `ZipCrypto` one by the
    /// dependency's own error alone.
    fn unsupported_form(self) -> Option<&'static str> {
        match self.aes? {
            AesMode::Aes128 => Some("AES-128"),
            AesMode::Aes192 => Some("AES-192"),
            AesMode::Aes256 => Some("AES-256"),
        }
    }
}

/// Every entry's stored name and encryption, in table order.
///
/// The name a container has settled — general-purpose bit 11, or an Info-ZIP Unicode Path
/// extra field — is taken as it stands and never reaches a chosen encoding. Both arrive here
/// as one flag, because the dependency sets `is_utf8` for the extra field too
/// (`read.rs:705-713`) after overwriting `file_name_raw` with its UTF-8 content. That is why
/// the raw bytes are "the best name bytes the crate has" rather than "the bytes in the central
/// directory", and why a decoder that assumed the latter would re-decode an already correct
/// name through `Shift_JIS`.
///
/// An entry whose local header cannot be parsed contributes the archive's own decode and is
/// left to fail when it is read, which is where it fails today. The survey refuses nothing:
/// a malformed *entry* is not a malformed archive.
fn survey<R: Read + Seek>(archive: &mut ZipArchive<R>) -> (Vec<Stated>, Vec<Encryption>) {
    let mut stated = Vec::with_capacity(archive.len());
    let mut encryption = Vec::with_capacity(archive.len());
    for position in 0..archive.len() {
        let guess = archive
            .name_for_index(position)
            .map_or_else(String::new, str::to_owned);
        let Ok(file) = archive.by_index_raw(position) else {
            stated.push(Stated::Decided(guess));
            encryption.push(Encryption {
                encrypted: false,
                aes: None,
            });
            continue;
        };
        let metadata = file.get_metadata();
        stated.push(if metadata.is_utf8 {
            Stated::Decided(guess)
        } else {
            Stated::Undecided {
                guess,
                bytes: metadata.file_name_raw.to_vec(),
            }
        });
        encryption.push(Encryption {
            encrypted: metadata.encrypted,
            aes: metadata.aes_mode.map(|(mode, ..)| mode),
        });
    }
    (stated, encryption)
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
