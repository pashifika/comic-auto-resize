//! Writing the output archive in read order.
//!
//! Output is zip whatever the input was, and entries land in the order they were read
//! whatever order the workers finished in. Writing them in completion order and recording
//! the intended order in the central directory was considered and rejected: readers present
//! entries in stored order, so the book would be shuffled for anyone whose viewer does not
//! sort by name.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
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
    archive: ZipWriter<File>,
    /// Completed pages above the one being waited for. Bounded by the pipeline's read-ahead
    /// window, not by the page count — see [`crate::pipeline`].
    pending: BTreeMap<PageKey, Page>,
    next_index: u32,
    written: u32,
    /// Where the archive is being built. Renamed onto the final path only on success, so a
    /// failed run leaves nothing at the destination.
    partial: PathBuf,
    final_path: PathBuf,
}

impl Sink {
    /// Creates the output archive, refusing to disturb an existing file at `path`.
    ///
    /// # Errors
    ///
    /// [`RunError::OutputExists`] when `path` is taken, and [`RunError::Io`] when the
    /// partial file cannot be created.
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

        let file = File::create(&partial).map_err(|source| RunError::Io {
            path: partial.clone(),
            source,
        })?;

        Ok(Self {
            archive: ZipWriter::new(file),
            pending: BTreeMap::new(),
            next_index: 0,
            written: 0,
            partial,
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
    /// # Errors
    ///
    /// [`RunError::Incomplete`] if a page never arrived, and [`RunError::Archive`] or
    /// [`RunError::Io`] if the archive cannot be closed or renamed.
    pub fn finish(self) -> Result<u32, RunError> {
        if let Some(key) = self.pending.keys().next().copied() {
            // Every page that was read is accounted for by the time the workers are joined,
            // so a leftover here means the ordering invariant broke rather than that a page
            // failed. Reported instead of silently writing a book with a gap.
            return Err(RunError::Incomplete {
                expected: self.next_index,
                stranded: key.0,
            });
        }

        self.archive.finish().map_err(RunError::Archive)?;
        fs::rename(&self.partial, &self.final_path).map_err(|source| RunError::Io {
            path: self.final_path.clone(),
            source,
        })?;
        Ok(self.written)
    }

    /// Removes the partial archive. Called when the run has already failed.
    pub fn abandon(self) {
        let partial = self.partial;
        drop(self.archive);
        // The run is already failing; a failure to clean up must not replace its error.
        let _ = fs::remove_file(&partial);
    }

    fn write(&mut self, page: &Page) -> Result<(), RunError> {
        // Stored, because entropy-coded JPEG does not compress: deflating it spends a pass
        // over every byte for no reduction.
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(false);
        self.archive
            .start_file(page.name.as_str(), options)
            .map_err(RunError::Archive)?;
        self.archive
            .write_all(&page.bytes)
            .map_err(|source| RunError::Io {
                path: self.partial.clone(),
                source,
            })?;
        self.written += 1;
        Ok(())
    }
}

/// The default output path for `input`: its stem plus `_resize.zip`, in its own directory.
///
/// The `_resize` suffix is load-bearing rather than cosmetic. Without it a `.zip` input
/// resolves to its own path, so the refusal to overwrite would fire on every run — and a
/// future `--delete-org` would destroy the input. With it the two can never coincide,
/// because the resolved name always ends in `_resize.zip` and always gains that suffix from
/// whatever stem it started with.
#[must_use]
pub fn default_output(input: &Path) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default();
    let mut name = stem.to_os_string();
    name.push("_resize.zip");
    input.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::default_output;
    use std::path::Path;

    #[test]
    fn the_default_output_is_the_stem_plus_resize_zip() {
        assert_eq!(
            default_output(Path::new("/books/foo.zip")),
            Path::new("/books/foo_resize.zip")
        );
        assert_eq!(
            default_output(Path::new("/books/foo.rar")),
            Path::new("/books/foo_resize.zip")
        );
        // No extension at all.
        assert_eq!(
            default_output(Path::new("/books/foo")),
            Path::new("/books/foo_resize.zip")
        );
        // Relative, staying in the input's directory.
        assert_eq!(
            default_output(Path::new("foo.zip")),
            Path::new("foo_resize.zip")
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
            let input = Path::new(input);
            assert_ne!(
                default_output(input),
                input,
                "{} resolved to itself",
                input.display()
            );
        }
    }
}
