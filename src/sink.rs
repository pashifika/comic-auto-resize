//! Writing the output archive in read order.
//!
//! Output is zip whatever the input was, and entries land in the order they were read
//! whatever order the workers finished in. Writing them in completion order and recording
//! the intended order in the central directory was considered and rejected: readers present
//! entries in stored order, so the book would be shuffled for anyone whose viewer does not
//! sort by name.

use std::collections::{BTreeMap, HashSet};
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
    /// Where the archive is being built. Renamed onto the final path only on success, so a
    /// failed run leaves nothing at the destination.
    partial: PathBuf,
    final_path: PathBuf,
    /// Every name written, so a collision created by extension rewriting is reported as
    /// itself rather than as `zip`'s "Duplicate filename".
    names: HashSet<String>,
    /// Set once the rename succeeded. Until then [`Sink::drop`] removes the partial, so no
    /// error path can leave a full-size stray file beside the destination.
    installed: bool,
}

impl Sink {
    /// Creates the output archive, refusing to disturb anything already at `path`.
    ///
    /// The partial file is created with `create_new`, so an existing path fails instead of
    /// being opened. That matters because the partial's name is derived from the output's and
    /// is therefore predictable: without exclusive creation, anyone able to write the output
    /// directory could pre-place `<output>.part` as a symbolic or hard link and have this
    /// process truncate and overwrite the link's target, then rename the link into place.
    ///
    /// # Errors
    ///
    /// [`RunError::OutputExists`] when `path` is taken, [`RunError::PartialExists`] when the
    /// partial is, and [`RunError::Io`] when it cannot be created.
    pub fn create(path: &Path) -> Result<Self, RunError> {
        if path.exists() {
            return Err(RunError::OutputExists {
                path: path.to_path_buf(),
            });
        }

        // Beside the destination, so the rename is within one directory and therefore
        // atomic on both release targets.
        let mut partial = path.as_os_str().to_os_string();
        partial.push(".part");
        let partial = PathBuf::from(partial);

        let file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
        {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return Err(RunError::PartialExists { path: partial });
            }
            Err(source) => {
                return Err(RunError::Io {
                    path: partial,
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
            partial,
            installed: false,
            final_path: path.to_path_buf(),
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

    /// Finishes the archive and moves it onto the final path.
    ///
    /// Every failure here leaves the partial for [`Sink::drop`] to remove, including a full
    /// disk while the central directory is written — which is exactly when leaving a
    /// full-size stray file would hurt most.
    ///
    /// # Errors
    ///
    /// [`RunError::Empty`] when no page was written, [`RunError::Incomplete`] if a page never
    /// arrived, and [`RunError::Archive`] or [`RunError::Io`] if the archive cannot be closed
    /// or renamed.
    pub fn finish(mut self) -> Result<u32, RunError> {
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
            // An archive with no pages in it is not an output worth installing: it would
            // report success, and then make the next run fail with "already exists".
            return Err(RunError::Empty);
        }

        let archive = self.archive.take().ok_or(RunError::StagePanicked {
            stage: "the writer",
        })?;
        archive.finish().map_err(RunError::Archive)?;
        fs::rename(&self.partial, &self.final_path).map_err(|source| RunError::Io {
            path: self.final_path.clone(),
            source,
        })?;
        self.installed = true;
        Ok(self.written)
    }

    fn write(&mut self, page: &Page) -> Result<(), RunError> {
        // Rewriting every extension to the encoder's can map two stored names onto one, and
        // widening the extension filter to png, bmp and webp widened this too: `p.jpeg` and
        // `p.jpg` collide, and so now do `cover.jpg` and `cover.png`. The second was silently
        // *skipped* before those formats were decoded, so an archive holding both used to
        // produce a short book and exit 0; refusing is the same rule this project applies to
        // every other name it cannot carry faithfully, and `--fix-idx` is the way through,
        // because a positional name takes the next number instead of the stem.
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
        let partial = self.partial.clone();
        let archive = self.archive.as_mut().ok_or(RunError::StagePanicked {
            stage: "the writer",
        })?;
        archive
            .start_file(page.name.as_str(), options)
            .map_err(RunError::Archive)?;
        archive
            .write_all(&page.bytes)
            .map_err(|source| RunError::Io {
                path: partial,
                source,
            })?;
        self.written += 1;
        Ok(())
    }
}

impl Drop for Sink {
    /// Removes the partial unless it was installed.
    ///
    /// This is the single cleanup path, rather than each exit doing its own: `finish`
    /// consumes the sink, so a failure inside it leaves the caller nothing to call, and a
    /// panic in the pipeline skips the caller entirely.
    fn drop(&mut self) {
        if self.installed {
            return;
        }
        // The run is already failing; a failure to clean up must not replace its error.
        let _ = fs::remove_file(&self.partial);
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

/// The default output path for `input`: its name plus `_resize.zip`, in its own directory.
///
/// The `_resize` suffix is load-bearing rather than cosmetic. Without it a `.zip` input
/// resolves to its own path, so the refusal to overwrite would fire on every run — and a
/// future `--delete-org` would destroy the input. With it the two can never coincide,
/// because the resolved name always ends in `_resize.zip` and always gains that suffix from
/// whatever stem it started with.
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
    let (base, mut name) = match kind {
        InputKind::Directory => {
            // Resolved once, not once per use: two `canonicalize` calls are two chances to
            // disagree if the tree moves between them. Nothing to remove from the name
            // either, because a directory has no extension.
            let base = resolved(input)?;
            let name = base
                .file_name()
                .map(std::ffi::OsString::from)
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
    // Beside the input rather than in it: `with_file_name` replaces the last component,
    // which for `/books/vol1` is `vol1` and gives `/books/vol1_resize.zip`.
    Ok(base.with_file_name(name))
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
    use super::{InputKind, default_output};
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
}
