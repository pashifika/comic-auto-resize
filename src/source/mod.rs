//! Reading an input as an ordered sequence of named pages.
//!
//! An enum rather than trait objects: the set of inputs is closed and known at compile time,
//! so a `match` costs nothing and every variant's reading discipline stays visible. All four
//! are here — zip, rar, 7z, and a plain directory, which is the one that is not an archive at
//! all and so the one that has to choose its own order.
//!
//! The enum carries no type parameter, and that is the decision Change 1 deferred. Only the
//! zip variant has a reader at all — `unrar` is handed a path, the directory variant a
//! directory, and 7z owns its own [`File`] on a decoder thread — and the only reader this
//! tool ever opens is a [`File`], because the input is a path on the command line. So
//! [`ZipSource`] stays generic, which is honest, and the enum names the one type production
//! uses. A generic here would have meant every later variant ignoring it.
//!
//! [`Entries`] is what the pipeline consumes. It exists so `pipeline::run` is not welded to
//! this enum: one test drives the pipeline over a [`ZipSource`] wrapping an instrumented
//! reader, to prove the credit window bounds the reader rather than the reader's own
//! restraint, and a concrete `ZipSource<File>` cannot be instrumented. Dispatch stays static.
//! Deliberately not `Iterator`, whose adapters would make `source.collect()` — the unbounded
//! buffering this pipeline exists to prevent — a one-liner that compiles.
//!
//! [`Entries`] stays pull-shaped even though 7z's decoder pushes. Flipping the trait would
//! not avoid the extra entry a push-shaped source holds — in a push shape every source reads
//! before it offers, so all four would pay it rather than one — and it would rewrite a trait
//! introduced one Change ago along with all its implementors. If a second push-only source
//! ever arrives, that trade changes and the trait should flip.

mod charset;
mod directory;
mod probe;
mod rar;
mod sevenz;
mod signature;
mod zip;

pub use charset::{BadLabel, Charset, DEFAULT_LABELS, Stated, Undecodable};
pub use directory::DirectorySource;
pub use probe::{
    CANDIDATES, Candidate, Format, MAGIC_MAX, Names, Naming, declared_format, output_name, probe,
};
pub use rar::RarSource;
pub use sevenz::{MAX_DICTIONARY_BYTES, SevenZSource};
pub use signature::{
    ARCHIVE_CANDIDATES, ARCHIVE_MAGIC_MAX, ArchiveCandidate, ArchiveFormat, detect,
    readable_formats,
};
pub use zip::ZipSource;

use std::fs::File;
use std::io::{Read, Seek};
use std::path::Path;

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
pub const MAX_ENTRY_BYTES: u64 = 64 << 20;

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

/// Hand-written rather than derived: `bytes` is a whole page, and a derive would put it in
/// every assertion failure and every log line. The length is what a reader of one wants.
impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("index", &self.index)
            .field("name", &self.name)
            .field("format", &self.format)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// What the pipeline needs from a source, and nothing more.
///
/// One method, because that is all `pipeline::run` calls. See the module doc for why this is
/// a trait rather than the enum, and why it is not `Iterator`.
pub trait Entries {
    /// The next page, or `None` at the end of the archive.
    ///
    /// Nothing is retained after it is returned, which is what lets an archive larger than
    /// memory be processed.
    fn next_entry(&mut self) -> Option<Result<Entry, SourceError>>;
}

/// What a reader needs from the command line, settled before the input is opened.
///
/// One struct rather than three parameters on five constructors, and the fields are what the
/// *rule* needs rather than what each format uses: only zip consults `charset`, and 7z
/// consults neither it nor `password`, because that is a property of those formats and not of
/// the options. A reader ignoring a field says so where it ignores it.
///
/// [`Default`] is today's behaviour with nothing chosen — stored names, no encoding, no
/// password. The guess is the command line's to make, so `--charset`'s non-empty default lives
/// in `main` and not here.
#[derive(Clone, Debug, Default)]
pub struct ReadOptions {
    pub naming: Naming,
    /// Encodings to choose from where the container declares none, in the order given. Empty
    /// means choose none, which is exactly the behaviour that preceded the option.
    pub charset: Charset,
    /// The password for an encrypted entry. `None` refuses one rather than reading its
    /// ciphertext as though it were a page.
    pub password: Option<String>,
}

/// An input being read once, in the order its entries are recorded in — or, for a directory,
/// in the order the reader chose.
pub enum Source {
    Zip(ZipSource<File>),
    Rar(RarSource),
    SevenZ(SevenZSource),
    Directory(DirectorySource),
}

/// Names the format and nothing else. A reader's internals are a cursor into an archive and
/// have no useful rendering; the variant is the part a diagnostic wants.
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Zip(_) => f.write_str("Source::Zip"),
            Self::Rar(_) => f.write_str("Source::Rar"),
            Self::SevenZ(_) => f.write_str("Source::SevenZ"),
            Self::Directory(_) => f.write_str("Source::Directory"),
        }
    }
}

impl Source {
    /// Opens the input at `path`, deciding its *kind* before its format.
    ///
    /// A directory is an input in its own right and has no leading bytes to probe, so the
    /// question "is this a directory" is asked first and the signature probe is reached only
    /// for a file. For a file the extension is not consulted; see [`signature`] for why. The
    /// whole decision lives here rather than in the binary so that "a rar named `.cbz` reads
    /// as rar" is a property a test can assert without running a process.
    ///
    /// `options` is threaded from here rather than read where each rule applies, because both
    /// halves need it before the first entry: the positional naming rule needs an entry total
    /// only each reader can supply, and the encoding rule needs every name in the container
    /// before the first page is handed on.
    ///
    /// # Errors
    ///
    /// [`SourceError::NotAnArchive`] when a file's leading bytes match no format this build
    /// reads, [`SourceError::NotReadable`] when the path is neither a file nor a directory,
    /// [`SourceError::Input`] when `path` cannot be opened or read, and whatever the chosen
    /// reader returns.
    pub fn open(path: &Path, options: &ReadOptions) -> Result<Self, SourceError> {
        let metadata = std::fs::metadata(path).map_err(|source| SourceError::Input { source })?;
        if metadata.is_dir() {
            return Self::directory(path, options);
        }
        if !metadata.is_file() {
            // A device, a socket, a fifo. Refused before it is opened, because opening a
            // fifo blocks until a writer appears and there is nothing to read from the rest.
            return Err(SourceError::NotReadable);
        }

        let mut file = File::open(path).map_err(|source| SourceError::Input { source })?;

        let mut header = [0; ARCHIVE_MAGIC_MAX];
        let read = fill(&mut file, &mut header).map_err(|source| SourceError::Input { source })?;

        match detect(&header[..read]) {
            Some(ArchiveFormat::Zip) => {
                // Rewound so the reader sees the whole archive.
                file.seek(std::io::SeekFrom::Start(0))
                    .map_err(|source| SourceError::Input { source })?;
                Self::zip(file, options)
            }
            Some(ArchiveFormat::Rar) => {
                // `unrar` is given the path, not a stream, so the probe handle has no further
                // use. Dropped explicitly: left to the end of this scope it would still be
                // open while `unrar` opened the same file a second time.
                drop(file);
                Self::rar(path, options)
            }
            Some(ArchiveFormat::SevenZ) => {
                file.seek(std::io::SeekFrom::Start(0))
                    .map_err(|source| SourceError::Input { source })?;
                Self::sevenz(file, options)
            }
            None => Err(SourceError::NotAnArchive {
                formats: readable_formats(),
            }),
        }
    }

    /// Opens `file` as a zip, reading its entry table and every stored name.
    ///
    /// # Errors
    ///
    /// [`SourceError::Archive`] when the entry table cannot be read, and
    /// [`SourceError::Charset`] when no listed encoding decodes every name — both established
    /// here rather than at the first entry.
    pub fn zip(file: File, options: &ReadOptions) -> Result<Self, SourceError> {
        Ok(Self::Zip(ZipSource::new(file, options)?))
    }

    /// Opens the rar archive at `path`, reading its archive header only.
    ///
    /// Unlike [`Source::zip`], this does not read the entry table: `unrar` walks headers as
    /// it goes, so a malformed *entry* header surfaces at the first [`Entries::next_entry`]
    /// rather than here. Not equalised, because equalising it would mean walking every header
    /// at open and then again to read — two passes over a solid archive, which is the one
    /// thing a solid archive makes expensive.
    ///
    /// # Errors
    ///
    /// [`SourceError::UnsafePath`] when `path` contains an interior NUL, which `unrar` panics
    /// on rather than reporting. [`SourceError::Rar`] when the archive header cannot be read.
    pub fn rar(path: &Path, options: &ReadOptions) -> Result<Self, SourceError> {
        Ok(Self::Rar(RarSource::open(path, options)?))
    }

    /// Opens `file` as a 7z, reading its header and starting the decoder thread.
    ///
    /// `options.charset` is not consulted and cannot be: 7z stores a name as UTF-16, so there
    /// are no legacy bytes to choose an encoding for. `options.password` is not consulted
    /// either, and that is a property of this build rather than of the format — see
    /// [`SevenZSource::new`].
    ///
    /// # Errors
    ///
    /// [`SourceError::SevenZ`] when the header cannot be read, and
    /// [`SourceError::Encrypted`] when the archive is encrypted.
    pub fn sevenz(file: File, options: &ReadOptions) -> Result<Self, SourceError> {
        Ok(Self::SevenZ(SevenZSource::new(file, options)?))
    }

    /// Lists the directory at `path`, choosing the order its pages will be read in.
    ///
    /// # Errors
    ///
    /// [`SourceError::Input`] when the directory itself cannot be listed,
    /// [`SourceError::Entry`] when one below it cannot, and the refusals the walk makes —
    /// see [`DirectorySource::open`].
    pub fn directory(path: &Path, options: &ReadOptions) -> Result<Self, SourceError> {
        Ok(Self::Directory(DirectorySource::open(path, options)?))
    }
}

impl Entries for Source {
    fn next_entry(&mut self) -> Option<Result<Entry, SourceError>> {
        match self {
            Self::Zip(source) => source.next_entry(),
            Self::Rar(source) => source.next_entry(),
            Self::SevenZ(source) => source.next_entry(),
            Self::Directory(source) => source.next_entry(),
        }
    }
}

/// So a test can drive the pipeline over an instrumented reader; see the module doc.
impl<R: Read + Seek> Entries for ZipSource<R> {
    fn next_entry(&mut self) -> Option<Result<Entry, SourceError>> {
        ZipSource::next_entry(self)
    }
}

/// How much capacity an entry's recorded size may reserve.
///
/// A real page is tens of kilobytes to a few megabytes, so a megabyte is a useful hint and
/// anything beyond it is the archive's claim rather than a measurement. Growth from there is
/// geometric.
pub(crate) const HINT_CEILING: u64 = 1 << 20;

/// Whether a stored name is a directory rather than a file.
///
/// The archive says so by ending the name with a separator, and both are separators because
/// a Windows-written archive may use either. The same rule `zip` applies internally, spelled
/// out here because its helper is crate-private.
///
/// rar marks a directory with a header flag and is checked on that first; this remains the
/// fallback there, because a directory entry without the flag is a malformed archive rather
/// than a page.
pub(crate) fn is_directory(name: &str) -> bool {
    matches!(name.as_bytes().last(), Some(b'/' | b'\\'))
}

/// Why a stored name must not be carried into the output archive, or `None` if it may be.
///
/// Every check is on the name as the archive stored it, before the extension is rewritten,
/// and both separators are treated as separators because a Windows-written archive may use
/// either.
///
/// Shared by every format on purpose: the refusal is a property of the name that reaches the
/// output, not of the container it came from.
pub(crate) fn unsafe_name(name: &str) -> Option<&'static str> {
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
    //
    // Checked per component, not just at the start, and that is the whole point: Windows
    // parses `safe/C:page.jpg` as a *drive-relative* path, so pushing it onto an extraction
    // root discards the root. A check anchored at byte zero lets exactly that through, which
    // is a name escaping the directory it was extracted into by a second spelling. The `..`
    // check below has always been per component; this one now matches it.
    //
    // A colon elsewhere in a component is *not* refused. `page.jpg:stream` names an NTFS
    // alternate data stream, which is a nuisance rather than an escape — it stays inside the
    // extraction root — and refusing every colon would refuse names that are ordinary on the
    // filesystem the archive came from.
    if name.split(['/', '\\']).any(|component| {
        let bytes = component.as_bytes();
        bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
    }) {
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

/// Reads up to `buffer.len()` bytes, tolerating short reads.
///
/// Not `read_exact`: an input shorter than the longest signature, or an entry shorter than
/// the longest magic, is not an I/O error — it is something that cannot be what was hoped
/// for, and the caller decides that from what was read.
///
/// Generic rather than `impl Read` so an unsized reader passes: 7z hands its callback a
/// `&mut dyn Read`, and the head is read off it exactly as it is off a zip entry.
pub(crate) fn fill<R: Read + ?Sized>(reader: &mut R, buffer: &mut [u8]) -> std::io::Result<usize> {
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

/// Why an archive could not be read.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SourceError {
    /// The archive's own structure, rather than one entry's: the entry table is read when
    /// the source is opened, so a truncated or malformed one is reported before any entry.
    /// An entry that cannot be read is [`SourceError::Entry`], which names it. Also carries
    /// the unreachable case in the reader where the entry table has no name for a position
    /// the table itself supplied.
    #[error("cannot read the archive: {0}")]
    Archive(#[from] ::zip::result::ZipError),
    /// The entry table kept fewer entries than the archive records. `zip` keys the table on
    /// the stored name, so two entries stored under one name byte for byte collapse into one
    /// record — and the page that lost would leave the book without a word. Refused rather
    /// than shortened: a book missing a page is the failure a reader notices last.
    #[error(
        "the archive records {recorded} entries but only {kept} can be addressed, which happens when two entries share a stored name"
    )]
    RepeatedName { recorded: u64, kept: u64 },
    /// No encoding in the user's list decodes every name the container left undeclared.
    /// Refused rather than fallen back from: the fallback is the format's historical default,
    /// which is what turns a page into a subdirectory, and a lossy decode would put U+FFFD in
    /// an output name.
    #[error(transparent)]
    Charset(#[from] Undecodable),
    /// The container encrypted this entry and no password was supplied. Refused rather than
    /// read: the alternative is ciphertext in the output under a page's name.
    #[error("{name}: the entry is encrypted; pass --pwd with the archive's password")]
    Encrypted { name: String },
    /// The container encrypted this entry in a form this build does not carry. Named by form,
    /// because "wrong password" is what the dependency says here and it is the wrong
    /// diagnosis: no password would have worked.
    ///
    /// *Declares* rather than *is*, because the two are separable: a zip entry can carry an
    /// AE-x extra field with its encryption flag clear, which is malformed rather than
    /// plaintext, and naming the form it declares is more use than asserting a state its own
    /// header denies.
    #[error("{name}: the entry declares {form} encryption, which this build cannot decrypt")]
    EncryptionUnsupported { name: String, form: &'static str },
    /// An encrypted entry that would not read after a password was supplied.
    ///
    /// The clause about one in 256 is not padding. `ZipCrypto` authenticates a password against
    /// a single byte, which upstream documents, so 255 wrong passwords in 256 are refused
    /// outright and the 256th is *accepted* — the data is then garbage and what surfaces is a
    /// page that will not decode. From here that is indistinguishable from a damaged archive,
    /// so the message names both rather than picking one.
    #[error(
        "{name}: {reason}, with a password supplied — this encryption form accepts one wrong password in 256, so the password may be wrong rather than the archive damaged"
    )]
    BadPassword { name: String, reason: String },
    #[error("{name}: cannot read the entry: {source}")]
    Entry {
        name: String,
        source: std::io::Error,
    },
    /// An entry the container lists but cannot yield at all: its record points at no entry
    /// header, so neither its data nor its stored name bytes are reachable.
    ///
    /// Separate from [`SourceError::Entry`] only for what it has to say about the name. Every
    /// other entry in the same archive is named by the encoding the reader chose; this one can
    /// only be named by the container's own decode, so the message says which it is rather
    /// than sending the reader to look for a page that exists under no such name.
    #[error(
        "{name}: cannot read the entry: {source}. That name is the container's own decoding of the stored bytes, which could not be read either"
    )]
    Unreachable {
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
    /// One entry could not be read, where the archive itself is fine — an encrypted entry in
    /// an archive whose headers are not encrypted is the case that reaches here. Named
    /// separately from [`SourceError::Rar`], which says "cannot read the archive" and would
    /// be saying it about an archive that reads.
    #[error("{name}: cannot read the entry: {source}")]
    RarEntry {
        name: String,
        source: unrar_ng::error::UnrarError,
    },
    /// The rar archive's own structure, the counterpart to [`SourceError::Archive`]. A
    /// separate variant only because `unrar`'s error type is not `zip`'s; the case is the
    /// same one.
    #[error("cannot read the archive: {0}")]
    Rar(#[from] unrar_ng::error::UnrarError),
    /// One volume of a multi-volume set was handed in. `unrar` would otherwise ask for the
    /// next volume and, on being refused, fail with an error that does not say what happened.
    /// Named here instead, because a half-followed volume set is a book missing pages and
    /// the user needs to know which failure they have.
    #[error(
        "{name}: this entry continues into another volume, so the input is one part of a multi-volume set"
    )]
    Split { name: String },
    /// The 7z archive's own structure, the counterpart to [`SourceError::Archive`]. A
    /// separate variant only because `sevenz-rust2`'s error type is neither `zip`'s nor
    /// `unrar`'s; the case is the same one. It also carries an archive whose headers are
    /// encrypted, and one written with a codec this build does not carry — `PPMd` and
    /// `BZip2` are licence-blocked rather than merely absent, so both arrive here.
    #[error("cannot read the archive: {0}")]
    SevenZ(#[from] sevenz_rust2::Error),
    /// The 7z decoder runs on a thread of the reader's making, and the pipeline's own
    /// `catch_unwind` sits on the outer reader thread where it would not see a panic here.
    /// Reported rather than lost, because a run that dies without a message is the worst
    /// kind to be handed.
    #[error("the 7z decoder stopped unexpectedly")]
    SevenZPanicked,
    /// A 7z block declares more decoder working memory than this build will allocate. The
    /// size is the archive's choice and the format lets it reach 4 GiB; the dependency's own
    /// guard cannot fire and exposes no knob, so the ceiling is applied from the header
    /// before any block is decoded.
    #[error(
        "the archive asks for a {declared}-byte decoder dictionary, more than the limit of {limit} bytes"
    )]
    Dictionary { declared: u64, limit: u64 },
    /// A directory holds something with a page's extension that is not a regular file — a
    /// fifo, a socket, a device. Refused rather than opened: opening a fifo blocks until a
    /// writer appears, and `Source::open` already refuses the same three as an *input*.
    #[error("{name}: refusing the entry because it is not a regular file")]
    NotAPage { name: String },
    /// A symbolic link inside a directory input. Not followed — its target may sit outside
    /// the input entirely — and refused rather than passed over, because the walk cannot
    /// tell a link to a page from a link to a chapter of them without resolving it, and a
    /// page that vanishes in silence is the failure this project refuses above all others.
    #[error("{name}: refusing the entry because it is a symbolic link, which is not followed")]
    SymbolicLink { name: String },
    /// The input is neither a file nor a directory: a device, a socket, a fifo. Refused
    /// before it is opened, because opening a fifo blocks until a writer appears.
    #[error("not a file or a directory")]
    NotReadable,
    /// The *input path*, not an entry name. `unrar` panics rather than erroring when the
    /// path it is given contains an interior NUL (`pathed/all.rs`, `WideCString::from_os_str`
    /// followed by `expect`), and a panic in the reader thread costs the run its message.
    /// Checked before the path reaches the dependency, so it is the error it should have been.
    ///
    /// Carries no path, for the reason below.
    #[error("refusing the input path because it contains a NUL byte")]
    UnsafePath,
    /// The input's leading bytes match no format this build reads. Names the formats,
    /// because "not a zip archive" was never the whole answer and is now actively wrong.
    #[error("not an archive this build reads ({formats})")]
    NotAnArchive { formats: String },
    /// The input path itself could not be opened or read, as distinct from an entry inside
    /// it.
    #[error("{source}")]
    Input { source: std::io::Error },
}

// None of these variants names the input path, and that is deliberate rather than an
// oversight: the caller passed the path in, and every one of them reaches a user through a
// wrapper that prepends it — `CliError::Archive` is `{path}: {source}`. Carrying it here too
// printed it twice. What each variant does name is the thing the caller could not have known,
// which for an entry is the entry.

#[cfg(test)]
mod tests {
    use super::unsafe_name;

    /// The name reaches the *output* archive, so what matters is what an extractor does with
    /// it — and the two spellings of a separator are both separators, because an archive
    /// written on Windows uses one and an archive written elsewhere the other.
    #[test]
    fn a_page_name_that_stays_where_it_is_put_is_allowed() {
        for name in [
            "page1.jpg",
            "pages/page1.jpg",
            "pages\\page1.jpg",
            "v1.2/page1.jpg",
            // A colon that is not a drive designator names an NTFS alternate data stream,
            // which stays inside the extraction root. A nuisance, not an escape.
            "page.jpg:stream",
            // A single dot is the current directory, not an escape.
            "./page1.jpg",
        ] {
            assert_eq!(unsafe_name(name), None, "{name}");
        }
    }

    /// A drive designator makes a Windows path drive-relative, which discards whatever root
    /// it is pushed onto. Anchored at byte zero, the check missed every nested spelling —
    /// which is the one an archive would carry, since the outer component looks harmless.
    ///
    /// Asserted here rather than only through a 7z fixture because the fixture cannot exist
    /// on Windows: creating `safe/C:page.jpg` on NTFS makes an alternate data stream of
    /// `safe\C`, not a file. The rule is platform-independent even though the file is not.
    #[test]
    fn a_drive_letter_in_any_component_is_refused() {
        for name in [
            "C:page.jpg",
            "C:/page.jpg",
            "safe/C:page.jpg",
            "safe\\C:page.jpg",
            "a/b/Z:evil.jpg",
        ] {
            assert_eq!(
                unsafe_name(name),
                Some("the name carries a drive letter"),
                "{name}"
            );
        }
    }

    #[test]
    fn a_name_that_escapes_or_is_absolute_is_refused() {
        assert_eq!(unsafe_name(""), Some("the name is empty"));
        assert_eq!(
            unsafe_name("a\0b.jpg"),
            Some("the name contains a NUL byte")
        );
        assert_eq!(unsafe_name("/abs.jpg"), Some("the name is absolute"));
        // A UNC prefix, which is absolute on Windows however it reads here.
        assert_eq!(
            unsafe_name("\\\\host\\share.jpg"),
            Some("the name is absolute")
        );
        for name in [
            "../page.jpg",
            "a/../../b.jpg",
            ".. /page.jpg",
            "a/.../b.jpg",
        ] {
            assert_eq!(
                unsafe_name(name),
                Some("the name escapes its own directory"),
                "{name}"
            );
        }
    }
}
