//! Writing the output archive in read order.
//!
//! Output is zip whatever the input was, and entries land in the order they were read
//! whatever order the workers finished in. Writing them in completion order and recording
//! the intended order in the central directory was considered and rejected: readers present
//! entries in stored order, so the book would be shuffled for anyone whose viewer does not
//! sort by name.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::pipeline::RunError;

/// Where a finished page belongs in the output.
///
/// `sub_index` is always zero today. It exists so that splitting a spread into two pieces
/// later needs no change to the ordering key.
pub type PageKey = (u32, u32);

/// One finished page, waiting for its turn.
pub struct Page {
    pub key: PageKey,
    pub name: String,
    pub bytes: Vec<u8>,
}

/// The output archive, fed in read order from completions that may arrive out of order.
pub struct Sink {
    /// `Option` only so `finish` can take it: `ZipWriter::finish` consumes, and `Sink` has a
    /// `Drop`. It is `Some` for the whole life of the sink until then.
    archive: Option<ZipWriter<File>>,
    /// Completed pages above the one being waited for. Bounded by the pipeline's read-ahead
    /// window, not by the page count — see [`crate::pipeline`].
    pending: BTreeMap<PageKey, Page>,
    next_index: u32,
    written: u32,
    /// The output's own path, claimed at creation and built in place.
    path: PathBuf,
    /// Every name written, so a collision created by extension rewriting is reported as
    /// itself rather than as `zip`'s "Duplicate filename".
    names: HashSet<String>,
    /// Set once the archive was closed and flushed. Until then [`Sink::drop`] removes the
    /// file, so no error path can leave a half-written archive at the destination.
    finished: bool,
}

impl Sink {
    /// Creates the output archive at `path`, refusing to disturb anything already there.
    ///
    /// **The exclusive creation of `path` is itself the refusal.** There is no separate
    /// existence check to race and no temporary name to rename from, because there is no
    /// rename: the archive is built in the file it will be delivered as. That closes three
    /// things at once.
    ///
    /// A check followed by a rename has a window — anything arriving at the destination in
    /// between was replaced, because `fs::rename` overwrites its destination by contract.
    /// `create_new` has no such window: the claim and the check are one operation the
    /// filesystem serialises.
    ///
    /// A second predictable name is a second thing to attack. Building in `<path>.part` meant
    /// anyone able to write the output directory could pre-place that name as a link and have
    /// this process write through it. One name is one surface.
    ///
    /// And a rename has to be *persisted* to be worth anything to `--delete-org`, which
    /// removes the input once the output is in place. No portable call makes a directory entry
    /// durable — on Windows `std::fs::rename` reaches `MoveFileExW` without
    /// `MOVEFILE_WRITE_THROUGH`, and `FlushFileBuffers` guarantees nothing for a directory
    /// handle — so a rename immediately before the unlink was a step whose durability could
    /// not be established. Claiming the name at the start does not *prove* the entry is
    /// durable either, and this is not claimed: what changes is that the entry is written
    /// before the whole conversion rather than microseconds before the input is removed.
    ///
    /// What it costs is that a run killed outright — power loss, `SIGKILL` — leaves an
    /// incomplete archive under the final name instead of under a `.part` one. The recovery is
    /// the same either way, since the next run refuses a name that is taken and the user
    /// removes the stray; what is lost is only that the stray no longer announces itself as
    /// incomplete.
    ///
    /// # Errors
    ///
    /// [`RunError::OutputExists`] when `path` is taken, and [`RunError::Io`] when it cannot be
    /// created for any other reason.
    pub fn create(path: &Path) -> Result<Self, RunError> {
        // `create_new` maps to `O_EXCL` and to `CREATE_NEW`, both of which fail on an existing
        // entry whatever it points at — including a dangling symbolic link, which
        // `Path::exists` used to answer `false` for by following it. The refusal is therefore
        // about the name, which is what the requirement is about, and it needs no separate
        // query whose failure would have to be interpreted.
        let file = match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return Err(RunError::OutputExists {
                    path: path.to_path_buf(),
                });
            }
            Err(source) => {
                return Err(RunError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        Ok(Self {
            archive: Some(ZipWriter::new(file)),
            pending: BTreeMap::new(),
            next_index: 0,
            written: 0,
            names: HashSet::new(),
            path: path.to_path_buf(),
            finished: false,
        })
    }

    /// Takes a finished page and writes everything that has become contiguous.
    ///
    /// Returns how many entries were written, which is how many read-ahead credits the
    /// pipeline may return.
    ///
    /// # Errors
    ///
    /// [`RunError::Archive`] or [`RunError::Io`] if the archive cannot be extended.
    pub fn accept(&mut self, page: Page) -> Result<u32, RunError> {
        self.pending.insert(page.key, page);

        let mut flushed = 0;
        // Only `sub_index` zero exists, so one entry advances the index by one. Splitting
        // would make this a range over the entry's piece count.
        while let Some(page) = self.pending.remove(&(self.next_index, 0)) {
            self.write(&page)?;
            self.next_index += 1;
            flushed += 1;
        }
        Ok(flushed)
    }

    /// Closes the archive and makes it durable, in the file it was built in.
    ///
    /// The flush is here rather than at the caller, and it is unconditional. Its cost is one
    /// `fsync` on a file this run just wrote, against a conversion measured in seconds to
    /// minutes; what it buys is that a successful return means the bytes are on the disk and
    /// not merely in the page cache, which is what `--delete-org` needs before it unlinks the
    /// only other copy — and what any consumer of a "written" message is entitled to assume.
    ///
    /// An earlier shape flushed only on the `--delete-org` path and did it by reopening the
    /// finished file. That charged nothing to other runs but made durability depend on the
    /// output being reopenable by its own creator, which a restrictive `umask` denies, and it
    /// left every other run's success line making a promise nothing had checked.
    ///
    /// Every failure here leaves the file for [`Sink::drop`] to remove, including a full disk
    /// while the central directory is written — which is exactly when leaving a full-size
    /// stray would hurt most.
    ///
    /// # Errors
    ///
    /// [`RunError::Empty`] when no page was written, [`RunError::Incomplete`] if a page never
    /// arrived, [`RunError::Archive`] if the archive cannot be closed, and [`RunError::Io`] if
    /// it cannot be flushed.
    pub fn finish(&mut self) -> Result<u32, RunError> {
        if let Some(key) = self.pending.keys().next().copied() {
            // Every page that was read is accounted for by the time the workers are joined,
            // so a leftover here means the ordering invariant broke rather than that a page
            // failed. Reported instead of silently writing a book with a gap.
            return Err(RunError::Incomplete {
                expected: self.next_index,
                stranded: key.0,
            });
        }
        if self.written == 0 {
            // An archive with no pages in it is not an output worth keeping: it would report
            // success, and then make the next run fail with "already exists".
            return Err(RunError::Empty);
        }

        let archive = self.archive.take().ok_or(RunError::StagePanicked {
            stage: "the writer",
        })?;
        // `ZipWriter::finish` writes the central directory and hands back the file it was
        // writing through, so the flush goes through the descriptor that produced the bytes.
        // No reopen, and therefore no dependence on the file's own mode.
        archive
            .finish()
            .map_err(RunError::Archive)?
            .sync_all()
            .map_err(|source| RunError::Io {
                path: self.path.clone(),
                source,
            })?;
        self.finished = true;
        Ok(self.written)
    }

    /// Removes the unfinished archive, reporting a failure to do so.
    ///
    /// The failure path's counterpart to [`Sink::finish`]. [`Sink::drop`] does the same
    /// removal and discards its error on purpose — a cleanup failure must not replace the
    /// failure that caused it — which is right for a panic and wrong for an ordinary error,
    /// because the archive is built under the output's own name and a stray one left there is
    /// what the next run will meet.
    ///
    /// # Errors
    ///
    /// Whatever the removal returned. The caller pairs it with the error that led here.
    pub fn abort(mut self) -> Result<(), std::io::Error> {
        // Taken so `Drop` does not try the removal a second time and swallow a second error.
        self.finished = true;
        drop(self.archive.take());
        match fs::remove_file(&self.path) {
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    fn write(&mut self, page: &Page) -> Result<(), RunError> {
        // Rewriting every extension to the encoder's can map two stored names onto one, and
        // widening the extension filter to png, bmp and webp widened this too: `p.jpeg` and
        // `p.jpg` collide, and so now do `cover.jpg` and `cover.png`. The second was silently
        // *skipped* before those formats were decoded, so an archive holding both used to
        // produce a short book and exit 0; refusing is the same rule this project applies to
        // every other name it cannot carry faithfully. `--fix-idx` is the way through for a
        // *numbered* stem, because the positional rule replaces its trailing digit run —
        // `cover.jpg` and `cover.png` have none, so they collide under it too.
        //
        // `ZipWriter` rejects the second with "Duplicate filename", which never mentions the
        // rename that caused it, so the collision is caught here where both halves are known.
        // The set costs no asymptotic memory the writer was not already paying: it holds every
        // name for the central directory regardless.
        if !self.names.insert(page.name.clone()) {
            return Err(RunError::NameCollision {
                name: page.name.clone(),
            });
        }

        // Stored, because entropy-coded JPEG does not compress: deflating it spends a pass
        // over every byte for no reduction.
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(false);
        let path = self.path.clone();
        let archive = self.archive.as_mut().ok_or(RunError::StagePanicked {
            stage: "the writer",
        })?;
        archive
            .start_file(page.name.as_str(), options)
            .map_err(RunError::Archive)?;
        archive
            .write_all(&page.bytes)
            .map_err(|source| RunError::Io { path, source })?;
        self.written += 1;
        Ok(())
    }
}

impl Drop for Sink {
    /// Removes the archive unless it was finished.
    ///
    /// This is the single cleanup path, rather than each exit doing its own: `finish` consumes
    /// the sink, so a failure inside it leaves the caller nothing to call, and a panic in the
    /// pipeline skips the caller entirely.
    ///
    /// It runs for an ordinary failure and for an unwind, and it cannot run for a process that
    /// is killed outright. That is the trade the in-place build accepts: a power loss or a
    /// `SIGKILL` leaves an incomplete archive under the output's own name, which the next run
    /// refuses as taken. Before, the same event left `<output>.part` and the next run refused
    /// that instead — the same recovery, a differently named stray.
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // The run is already failing; a failure to clean up must not replace its error.
        let _ = fs::remove_file(&self.path);
    }
}

/// What the input is, which decides how its output is named.
///
/// A file has an extension to remove and a directory does not, so the two cannot share one
/// derivation without stripping something a directory never had — `[Author] Title v1.5`
/// would lose its `.5`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputKind {
    File,
    Directory,
}

/// The output path for `input`, given whatever `-o` supplied.
///
/// `requested` resolves two ways, and the split is where this tool's responsibility ends. A
/// value naming a **location** — its final character is a path separator, or it already
/// exists and is a directory — is that directory joined with [`default_output`]'s name.
/// Anything else is a **filename** and is used verbatim: no extension appended, replaced or
/// validated, so `-o out.cbz` writes a zip archive called `out.cbz`. Naming only a location
/// is not a mistake, so the filename is supplied; naming a file is a decision, so it is not
/// second-guessed. The reference tool appends `.zip` to the value and refuses an existing
/// directory outright; both follow from the defect the default name corrects.
///
/// The default name is derived only where `requested` supplies none, so `car / -o out.zip`
/// never meets a refusal about a stem it did not need.
///
/// # Errors
///
/// [`RunError::MissingOutputDirectory`] when the directory to write into is not there. The
/// tool declines to create it, and the containment check needs a path that canonicalises,
/// which a directory that does not exist has none of.
///
/// [`RunError::OutputInsideInput`] when the resolved path lands inside a directory input,
/// where the next run would read it as a page. Both arms reach in — `-o vol1/out.zip` names
/// a filename inside the input and `-o vol1/` joins to one — so the bound is on the resolved
/// path rather than on either arm.
///
/// [`RunError::UnnamedInput`] as [`default_output`], and [`RunError::Io`] when a path that
/// exists cannot be canonicalised.
pub fn resolve_output(
    input: &Path,
    kind: InputKind,
    requested: Option<&Path>,
) -> Result<PathBuf, RunError> {
    let Some(requested) = requested else {
        // Nothing to bound: the default is placed beside the input rather than in it, and
        // its directory is one the input is already in.
        return default_output(input, kind);
    };

    let output = if is_location(requested) {
        let (_, name) = default_parts(input, kind)?;
        requested.join(name)
    } else {
        requested.to_path_buf()
    };

    let directory = output_directory(&output);
    if !directory.is_dir() {
        return Err(RunError::MissingOutputDirectory {
            path: directory.to_path_buf(),
        });
    }
    if kind == InputKind::Directory {
        refuse_inside_input(input, &output, directory)?;
    }
    Ok(output)
}

/// The default output path for `input`: its name plus `_resize.zip`, in its own directory.
///
/// The `_resize` suffix is load-bearing rather than cosmetic. Without it a `.zip` input
/// resolves to its own path, so the refusal to overwrite would fire on every run — and
/// `--delete-org` would destroy the input. With it the two can never coincide, because the
/// resolved name always ends in `_resize.zip` and always gains that suffix from whatever
/// stem it started with.
///
/// For a directory the suffix is load-bearing a second time. `vol1` and `vol1.zip` do not
/// collide, so nothing else would stop the output being written *inside* the input, where the
/// next run would read it as a page.
///
/// # Errors
///
/// [`RunError::UnnamedInput`] when a directory input has no name to derive one from, which
/// `.`, `..` and `/` are. Resolved against the filesystem first, so `.` names the directory
/// the user is standing in rather than nothing.
pub fn default_output(input: &Path, kind: InputKind) -> Result<PathBuf, RunError> {
    let (base, name) = default_parts(input, kind)?;
    // Beside the input rather than in it: `with_file_name` replaces the last component,
    // which for `/books/vol1` is `vol1` and gives `/books/vol1_resize.zip`.
    Ok(base.with_file_name(name))
}

/// The default output's name, and the path it is placed beside.
///
/// Split out because `-o` naming a location needs the name without the placement.
fn default_parts(input: &Path, kind: InputKind) -> Result<(PathBuf, OsString), RunError> {
    let (base, mut name) = match kind {
        InputKind::Directory => {
            // Resolved once, not once per use: two `canonicalize` calls are two chances to
            // disagree if the tree moves between them. Nothing to remove from the name
            // either, because a directory has no extension.
            let base = resolved(input)?;
            let name =
                base.file_name()
                    .map(OsString::from)
                    .ok_or_else(|| RunError::UnnamedInput {
                        path: input.to_path_buf(),
                    })?;
            (base, name)
        }
        InputKind::File => (
            input.to_path_buf(),
            input.file_stem().unwrap_or_default().to_os_string(),
        ),
    };
    name.push("_resize.zip");
    Ok((base, name))
}

/// Whether `value` names a location rather than a file.
///
/// The trailing separator has to be read before the value becomes a `Path`, because `Path`
/// normalises it away: `Path::new("dir/").file_name()` is `Some("dir")` and `components()`
/// drops it. `is_separator` is platform-correct, which matters because `\` is one on Windows
/// and is not on macOS, and `to_string_lossy` cannot change the answer — a replacement
/// character appears only where an invalid sequence was, and a trailing ASCII separator is
/// not one.
fn is_location(value: &Path) -> bool {
    value
        .as_os_str()
        .to_string_lossy()
        .ends_with(std::path::is_separator)
        || fs::metadata(value).is_ok_and(|meta| meta.is_dir())
}

/// The directory `output` would be written into.
///
/// `Path::parent` of a bare file name is the empty path, which names the current directory
/// but answers neither `is_dir` nor `canonicalize`, so it is spelled `.` here.
fn output_directory(output: &Path) -> &Path {
    match output.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Flushes the directory entry naming `output`, where the platform offers a way to.
///
/// [`Sink::finish`] flushes the archive's bytes through the handle that wrote them. That says
/// nothing about the entry that names them: on unix `fsync` on a file does not persist its
/// parent directory, so the name can still be lost to a crash while the data survives. Called
/// only before the input is removed, which is the one moment losing the name would leave
/// nothing to find the data by.
///
/// Unix only, and the gap is stated rather than papered over. `std::fs::rename` is not
/// involved any more — the name was claimed by the creation, long before this — but a Windows
/// directory handle still has no documented flush: `FlushFileBuffers` is specified for file
/// and volume handles and guarantees nothing for a directory one. What changed is that the
/// unflushed entry is now minutes old rather than microseconds old, which is a different
/// exposure, not a closed one.
///
/// # Errors
///
/// Whatever opening or flushing the directory returned. A failure means nothing may be
/// removed.
pub fn durable_directory_entry(output: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(output_directory(output))?.sync_all()?;
    #[cfg(not(unix))]
    let _ = output;
    Ok(())
}

/// Refuses a resolved output inside a directory input.
///
/// Canonical rather than lexical: `-o vol1/../vol1/out.zip` and a symbolic link into the tree
/// both spell a contained path without looking like one. Two `canonicalize` calls, but they
/// feed one comparison rather than two derivations, so the disagreement [`default_parts`]
/// guards against has nothing to divide here. Both paths exist by the time this runs — the
/// input is open, and the output's directory was just checked.
fn refuse_inside_input(input: &Path, output: &Path, directory: &Path) -> Result<(), RunError> {
    let tree = fs::canonicalize(input).map_err(|source| RunError::Io {
        path: input.to_path_buf(),
        source,
    })?;
    let into = fs::canonicalize(directory).map_err(|source| RunError::Io {
        path: directory.to_path_buf(),
        source,
    })?;
    if into.starts_with(&tree) {
        return Err(RunError::OutputInsideInput {
            path: output.to_path_buf(),
            input: input.to_path_buf(),
        });
    }
    Ok(())
}

/// `input` with `.`, `..` and any link resolved away, so it has a last component to work
/// from. Only reached for a directory: a file path is used exactly as it was given.
fn resolved(input: &Path) -> Result<PathBuf, RunError> {
    if input.file_name().is_some() {
        return Ok(input.to_path_buf());
    }
    fs::canonicalize(input).map_err(|source| RunError::Io {
        path: input.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::{InputKind, default_output, durable_directory_entry, output_directory};
    use std::path::{Path, PathBuf};

    fn file(input: &str) -> PathBuf {
        default_output(Path::new(input), InputKind::File).expect("a file always has a name")
    }

    fn directory(input: &str) -> PathBuf {
        default_output(Path::new(input), InputKind::Directory).expect("a named directory")
    }

    #[test]
    fn the_default_output_is_the_stem_plus_resize_zip() {
        assert_eq!(file("/books/foo.zip"), Path::new("/books/foo_resize.zip"));
        assert_eq!(file("/books/foo.rar"), Path::new("/books/foo_resize.zip"));
        // No extension at all.
        assert_eq!(file("/books/foo"), Path::new("/books/foo_resize.zip"));
        // Relative, staying in the input's directory.
        assert_eq!(file("foo.zip"), Path::new("foo_resize.zip"));
    }

    /// A directory has no extension to remove, so a dot in its name is part of the name.
    #[test]
    fn a_directory_keeps_every_part_of_its_own_name() {
        assert_eq!(
            directory("/books/vol1"),
            Path::new("/books/vol1_resize.zip")
        );
        assert_eq!(
            directory("/books/[Author] Title v1.5"),
            Path::new("/books/[Author] Title v1.5_resize.zip")
        );
        // A trailing separator names the same directory.
        assert_eq!(
            directory("/books/vol1/"),
            Path::new("/books/vol1_resize.zip")
        );
    }

    /// The output must not land inside the input, where a second run would read it as a page.
    #[test]
    fn a_directorys_output_is_written_beside_it() {
        let output = directory("/books/vol1");
        assert!(
            !output.starts_with("/books/vol1"),
            "{} is inside its own input",
            output.display()
        );
    }

    #[test]
    fn the_output_never_equals_the_input() {
        for input in [
            "/books/foo.zip",
            "/books/foo_resize.zip",
            "/books/foo",
            "foo.cbz",
            "/books/foo.tar.gz",
        ] {
            assert_ne!(
                file(input),
                Path::new(input),
                "{input} resolved to itself as a file"
            );
            assert_ne!(
                directory(input),
                Path::new(input),
                "{input} resolved to itself as a directory"
            );
        }
    }

    /// The directory barrier reports a failure rather than swallowing one.
    ///
    /// It is the half of durability the file's own flush cannot supply: `fsync` on a file says
    /// nothing about the entry naming it. A directory that is not there is the reachable
    /// failure, and it must come back as one — a barrier that quietly returns `Ok` is worse
    /// than none, because `--delete-org` unlinks the only other copy on the strength of it.
    #[test]
    fn the_directory_barrier_reports_a_failure_rather_than_swallowing_it() {
        let scratch = std::env::temp_dir().join(format!(
            "comic-auto-resize-durable-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&scratch).expect("creates the scratch directory");

        let written = scratch.join("out.zip");
        std::fs::write(&written, b"PK\x05\x06").expect("writes an archive");
        durable_directory_entry(&written).expect("an existing directory is flushed");

        // Unix only, because that is where the call exists at all; elsewhere the function is
        // documented as a no-op and there is nothing to fail.
        #[cfg(unix)]
        {
            let nowhere = scratch.join("gone").join("out.zip");
            let error = durable_directory_entry(&nowhere)
                .expect_err("a directory that is not there must not report success");
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "{error}");
        }

        std::fs::remove_dir_all(&scratch).expect("removes the scratch directory");
    }

    /// The empty-parent rule has one implementation, and the barrier shares it.
    ///
    /// `Path::parent` of a bare file name is the empty path, which names the current directory
    /// but answers neither `is_dir` nor `canonicalize`. Both the resolution and the flush need
    /// that spelled `.`, and a second copy of the rule is a second thing to get wrong.
    #[test]
    fn a_bare_file_name_names_the_current_directory() {
        assert_eq!(output_directory(Path::new("out.zip")), Path::new("."));
        assert_eq!(
            output_directory(Path::new("dest/out.zip")),
            Path::new("dest")
        );
        assert_eq!(output_directory(Path::new("/out.zip")), Path::new("/"));
    }
}
