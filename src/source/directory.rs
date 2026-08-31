//! Reading a plain directory as an ordered sequence of named pages.
//!
//! The degenerate input: no container, and — the part that matters — no stored order. Every
//! ordering requirement so far has been able to say "the order the archive recorded". A
//! directory records none, so the reader chooses one, and the choice is the substance of this
//! module.
//!
//! # The order is numeric-aware, and that is a decision rather than a default
//!
//! The Go implementation walked a directory with `fs.WalkDir` and applied no comparison of its
//! own (`utils/archiver/fs.go:60`), and its writer emitted the walk order verbatim
//! (`archiver/compressor.go`). `fs.WalkDir` sorts byte-lexically, so Go shipped `page1.jpg`,
//! `page10.jpg`, `page2.jpg` — a book with its pages out of sequence, silently. Confirmed
//! twice, by reading the walk and by reading the writer; there is no padding or natural-sort
//! logic anywhere in the Go tool or in `pashifika/util`.
//!
//! This reader compares runs of ASCII digits by value and everything else byte-wise, so
//! `page2` precedes `page10`. Not locale-aware: an order that depends on the machine that
//! produced it is the same class of problem as the charset loss Change 2 recorded.
//!
//! Recursion is depth-first with the path prefix preserved, matching Go's shape, and
//! subdirectories sort among the files beside them by the same key rather than as a separate
//! group — a chapter directory and a cover file are both things the reader has to place, and
//! one key places both.
//!
//! # Names are relative to the input directory, which is how Go unified the two input kinds
//!
//! Go turned both kinds into an `fs.FS` rooted at `.`: `os.DirFS(path)` for a directory, and a
//! synthesised root entry named `DefaultArchiverRoot` — `"."` — for an archive
//! (`compress-master/fs.go:41-44`, `interface.go:29`). One `fs.WalkDir(fsys, ".")` walked
//! either, so a walk path was always relative to the root.
//!
//! The same rule here: `~/books/vol1/` holding `page1.jpg` yields the entry name `page1.jpg`,
//! and `ch1/page1.jpg` yields `ch1/page1.jpg`. The directory's own name never enters an entry
//! name — it appears only in the output file name, `vol1_resize.zip`. Prefixing every entry
//! with it would put a component in the output that the user did not ask for and that no
//! archive input produces.
//!
//! # What is passed over, and what is refused
//!
//! A name beginning with a dot is passed over, which generalises Go's special case for
//! `.git`. An entry no candidate extension claims is passed over, as in every other reader.
//!
//! A symbolic link is never followed and is refused, naming itself. Not following one is what
//! keeps the output describing the files the input names, and refusing rather than passing
//! over follows from not following: the walk cannot tell a link to a page from a link to a
//! chapter of them without resolving it, so passing links over could drop a page — or a
//! chapter — in silence, which is the failure this project refuses above all others. The
//! stricter rule costs a loud error on a stray link to a readme; the looser one costs a book
//! with pages missing and a success message. Nothing here canonicalises a path or reasons
//! about where a link points, so it cannot loop either.
//!
//! A filesystem produces unsafe names as readily as a crafted archive does — a directory
//! literally named `C:` is legal on unix and absolute on Windows, and a file name may hold a
//! backslash, which is a separator there. So [`unsafe_name`] runs on the constructed entry
//! name exactly as it does for an archive's stored name. A name that is not valid UTF-8 is
//! refused by the same rule, because a zip entry name is UTF-8 and rewriting one would
//! produce an output whose entries do not match the input's.
//!
//! # The listing is held, and it is the same shape as an entry table
//!
//! Choosing an order means listing before yielding, so this reader holds one name per page for
//! the length of the run. That is what `ZipArchive`'s entry table and 7z's header already do,
//! and it is a name rather than a page: tens of bytes against the megabytes the pipeline's
//! window governs.

use std::cmp::Ordering;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::probe::{self, Format, MAGIC_MAX, Names, Naming};
use super::{Entry, HINT_CEILING, MAX_ENTRY_BYTES, ReadOptions, SourceError, fill, unsafe_name};

/// A directory listed once, then read in the order the listing chose.
pub struct DirectorySource {
    /// Every candidate page, in read order: the entry name relative to the input directory,
    /// and the path to open it by.
    pages: std::vec::IntoIter<Page>,
    next_index: u32,
    names: Names,
}

/// One file the listing accepted as a candidate page.
struct Page {
    /// Relative to the input directory, with `/` separators as an archive would store them.
    name: String,
    path: PathBuf,
    /// What the extension claimed at listing time, so the read does not ask twice.
    declared: Format,
}

impl DirectorySource {
    /// Lists `root`, choosing the order every entry will be read in.
    ///
    /// # Errors
    ///
    /// [`SourceError::Input`] when the input directory itself cannot be read and
    /// [`SourceError::Entry`] when one below it cannot, naming which;
    /// [`SourceError::SymbolicLink`] for a link, which is never followed;
    /// [`SourceError::NotAPage`] for something a candidate extension claims that is not a
    /// regular file; and [`SourceError::UnsafeName`] for a name the walk built that cannot be
    /// carried into the output archive. All are established here, before the output file
    /// exists, because the listing happens before anything is read.
    ///
    /// Neither `options.charset` nor `options.password` applies: a filesystem hands back a
    /// name already decoded, and a directory of pages has nothing to decrypt.
    pub fn open(root: &Path, options: &ReadOptions) -> Result<Self, SourceError> {
        let mut pages = Vec::new();
        walk(root, "", &mut pages)?;

        let names = match options.naming {
            Naming::Stored => Names::stored(),
            // The listing is made anyway, so the entry total is free. It counts the pages the
            // extension filter kept rather than every file, which is the one format where an
            // exact candidate count costs nothing.
            Naming::ByPosition => Names::by_position(pages.len()),
        };

        Ok(Self {
            pages: pages.into_iter(),
            next_index: 0,
            names,
        })
    }

    /// The next page, or `None` at the end of the listing.
    pub fn next_entry(&mut self) -> Option<Result<Entry, SourceError>> {
        let page = self.pages.next()?;
        Some(self.read(&page))
    }

    fn read(&mut self, page: &Page) -> Result<Entry, SourceError> {
        let Page {
            name,
            path,
            declared,
        } = page;
        let declared = *declared;

        // Re-checked here, not only in the walk. The listing runs at open and this runs a
        // page later, so a tree the tool does not own can change in between: a page can
        // become a symbolic link to something outside the input, and `File::open` follows one.
        //
        // This narrows that window and does not close it, and the two halves are worth
        // separating. For the *last* component `symlink_metadata` does not follow, so a swap
        // has to land between this call and the open below rather than anywhere in the run.
        // For an *ancestor* it resolves as any other path call does, so replacing a listed
        // `ch1/` with a link to somewhere else defeats the check outright.
        //
        // Closing either needs an open that does not follow — `openat2` with
        // `RESOLVE_NO_SYMLINKS` on unix, reparse-point protection on Windows — which needs a
        // platform crate on both release targets. Recorded in `archive-source` and in the
        // Change's evidence rather than left to be discovered, because the threat is a tree
        // being mutated under the tool while it runs, and the alternative to the check is no
        // check at all.
        let kind = std::fs::symlink_metadata(path)
            .map_err(|source| SourceError::Entry {
                name: name.clone(),
                source,
            })?
            .file_type();
        if kind.is_symlink() {
            return Err(SourceError::SymbolicLink { name: name.clone() });
        }
        if !kind.is_file() {
            return Err(SourceError::NotAPage { name: name.clone() });
        }

        let mut file = File::open(path).map_err(|source| SourceError::Entry {
            name: name.clone(),
            source,
        })?;

        // The filesystem records the size away from the data, so an over-large file costs
        // nothing to refuse and is not read at all.
        let recorded = file
            .metadata()
            .map_err(|source| SourceError::Entry {
                name: name.clone(),
                source,
            })?
            .len();
        if recorded > MAX_ENTRY_BYTES {
            return Err(SourceError::TooLarge {
                name: name.clone(),
                limit: MAX_ENTRY_BYTES,
            });
        }

        let mut head = [0; MAGIC_MAX];
        let head = match fill(&mut file, &mut head) {
            Ok(read) => &head[..read],
            Err(source) => {
                return Err(SourceError::Entry {
                    name: name.clone(),
                    source,
                });
            }
        };
        match probe::probe(head) {
            Some(format) if format == declared => {}
            _ => {
                return Err(SourceError::Mismatch {
                    name: name.clone(),
                    declared: declared.name(),
                });
            }
        }

        // Capped for the reason `zip.rs` caps it, and bounded on the read for the same reason
        // too: a file can grow between the `metadata` call and the read, so a check that
        // trusts the number it is validating is not a check.
        let hint = usize::try_from(recorded.min(HINT_CEILING)).unwrap_or(0);
        let mut bytes = Vec::with_capacity(hint.saturating_add(head.len()));
        bytes.extend_from_slice(head);
        let remaining = MAX_ENTRY_BYTES
            .saturating_sub(bytes.len() as u64)
            .saturating_add(1);
        file.take(remaining)
            .read_to_end(&mut bytes)
            .map_err(|source| SourceError::Entry {
                name: name.clone(),
                source,
            })?;
        if bytes.len() as u64 > MAX_ENTRY_BYTES {
            return Err(SourceError::TooLarge {
                name: name.clone(),
                limit: MAX_ENTRY_BYTES,
            });
        }

        let index = self.next_index;
        self.next_index += 1;
        Ok(Entry {
            index,
            name: self.names.of(name, declared),
            format: declared,
            bytes,
        })
    }
}

/// Appends every candidate page under `directory` to `pages`, in read order.
///
/// `prefix` is the entry-name prefix for this level, already ending in `/` when it is not
/// empty, so a name is built by concatenation rather than by path arithmetic.
fn walk(directory: &Path, prefix: &str, pages: &mut Vec<Page>) -> Result<(), SourceError> {
    // A failure below the root names the entry it happened at rather than the input root:
    // `main` prefixes the input path, so a bare `Input` error would print
    // `~/books/vol1: Permission denied` for an unreadable `~/books/vol1/ch3/private` and name
    // a directory that reads fine. At the root there is no name to add — the prefix has
    // already said it — so that one case stays as it was.
    let named = |source: std::io::Error, at: String| {
        if at.is_empty() {
            SourceError::Input { source }
        } else {
            SourceError::Entry { name: at, source }
        }
    };
    let here = || prefix.trim_end_matches('/').to_owned();

    let mut children = Vec::new();
    let listing = directory
        .read_dir()
        .map_err(|source| named(source, here()))?;
    for child in listing {
        let child = child.map_err(|source| named(source, here()))?;
        let raw = child.file_name();
        // Generalises Go's `.git` case: a dot-name is machinery rather than a page, and
        // descending into one would put `.DS_Store` and `.git` objects in the listing. Read
        // off the raw bytes, so a name that is not UTF-8 is still recognised as hidden.
        if raw.as_encoded_bytes().first() == Some(&b'.') {
            continue;
        }
        let Ok(name) = raw.into_string() else {
            // A zip entry name is UTF-8, so a name that is not would have to be rewritten to
            // reach the output. Refused rather than rewritten, for the reason every other
            // name refusal here is: an output whose entries do not match the input's is
            // worse than a run that stops and says so.
            return Err(SourceError::UnsafeName {
                name: format!("{prefix}{}", child.file_name().to_string_lossy()),
                reason: "the name is not valid UTF-8",
            });
        };
        // `file_type` on a `DirEntry` does not follow a link, which is the whole point: a
        // link's target may sit outside the input entirely.
        let kind = child
            .file_type()
            .map_err(|source| named(source, format!("{prefix}{name}")))?;
        children.push((name, child.path(), kind));
    }

    // Files and subdirectories together, by one key. `chapter2/` against `cover.jpg` is a
    // question the walk has to answer either way, and answering it with the same comparison
    // the pages use is the only answer that does not need a second rule.
    children.sort_by(|left, right| natural_cmp(&left.0, &right.0));

    for (name, path, kind) in children {
        let entry_name = format!("{prefix}{name}");
        // Before the extension filter and before the descent, because a link is refused for
        // what it might be rather than for what it is called. Not following one means the
        // walk cannot tell a link to a page from a link to a chapter of them, so refusing
        // every link is the only rule that cannot drop a page in silence — and silence is
        // the failure this reader exists to prevent. It also cannot loop.
        if kind.is_symlink() {
            return Err(SourceError::SymbolicLink { name: entry_name });
        }
        if kind.is_dir() {
            walk(&path, &format!("{entry_name}/"), pages)?;
            continue;
        }
        let Some(declared) = probe::declared_format(&entry_name) else {
            continue;
        };
        // Only a regular file is a page. `Source::open` refuses a fifo, a socket and a device
        // as an *input* because opening a fifo blocks until a writer appears; a child of a
        // directory input reaches the same `File::open` and needs the same refusal. Named
        // rather than passed over: the extension claimed it was a page, so it is a page this
        // run cannot read rather than a file this run has no interest in.
        if !kind.is_file() {
            return Err(SourceError::NotAPage { name: entry_name });
        }
        if let Some(reason) = unsafe_name(&entry_name) {
            return Err(SourceError::UnsafeName {
                name: entry_name,
                reason,
            });
        }
        pages.push(Page {
            name: entry_name,
            path,
            declared,
        });
    }
    Ok(())
}

/// Orders two names with runs of ASCII digits compared by value.
///
/// Byte-wise everywhere else, so nothing here consults a locale and the order is the same on
/// every host. Digit runs are compared by length after leading zeros are dropped rather than
/// by parsing, so a run longer than any integer type still orders correctly; two runs of equal
/// value are ordered by how many leading zeros they carry, which keeps the comparison a total
/// order rather than declaring `page1` and `page01` interchangeable.
fn natural_cmp(left: &str, right: &str) -> Ordering {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    let (mut at_left, mut at_right) = (0, 0);

    while at_left < left.len() && at_right < right.len() {
        if left[at_left].is_ascii_digit() && right[at_right].is_ascii_digit() {
            let end_left = digit_run(left, at_left);
            let end_right = digit_run(right, at_right);
            let run_left = &left[at_left..end_left];
            let run_right = &right[at_right..end_right];
            let value_left = trim_zeros(run_left);
            let value_right = trim_zeros(run_right);

            let by_value = value_left
                .len()
                .cmp(&value_right.len())
                .then_with(|| value_left.cmp(value_right));
            if by_value != Ordering::Equal {
                return by_value;
            }
            let by_padding =
                (run_left.len() - value_left.len()).cmp(&(run_right.len() - value_right.len()));
            if by_padding != Ordering::Equal {
                return by_padding;
            }

            at_left = end_left;
            at_right = end_right;
        } else {
            let by_byte = left[at_left].cmp(&right[at_right]);
            if by_byte != Ordering::Equal {
                return by_byte;
            }
            at_left += 1;
            at_right += 1;
        }
    }

    (left.len() - at_left).cmp(&(right.len() - at_right))
}

/// Where the run of ASCII digits starting at `from` ends.
fn digit_run(bytes: &[u8], from: usize) -> usize {
    let mut end = from;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    end
}

/// `run` without its leading zeros, keeping one digit when the run is all zeros.
fn trim_zeros(run: &[u8]) -> &[u8] {
    let leading = run.iter().take_while(|&&byte| byte == b'0').count();
    if leading == run.len() {
        &run[run.len() - 1..]
    } else {
        &run[leading..]
    }
}

#[cfg(test)]
mod tests {
    use super::natural_cmp;
    use std::cmp::Ordering;

    fn sorted(names: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = names.iter().map(|&name| name.to_owned()).collect();
        names.sort_by(|left, right| natural_cmp(left, right));
        names
    }

    /// The defect the Go implementation shipped: byte-lexical order puts `page10` second.
    #[test]
    fn a_digit_run_compares_by_value() {
        assert_eq!(
            sorted(&["page10.jpg", "page2.jpg", "page1.jpg"]),
            ["page1.jpg", "page2.jpg", "page10.jpg"]
        );
    }

    #[test]
    fn padding_does_not_change_the_value_it_carries() {
        assert_eq!(
            sorted(&["page010.jpg", "page9.jpg", "page0002.jpg"]),
            ["page0002.jpg", "page9.jpg", "page010.jpg"]
        );
    }

    /// Equal value, different spelling: ordered rather than declared equal, so the sort is a
    /// total order and two such names cannot swap between runs.
    #[test]
    fn two_spellings_of_one_number_are_ordered_not_equal() {
        assert_eq!(natural_cmp("page1.jpg", "page01.jpg"), Ordering::Less);
        assert_ne!(natural_cmp("page1.jpg", "page01.jpg"), Ordering::Equal);
    }

    #[test]
    fn a_run_longer_than_any_integer_still_orders() {
        let long = format!("page{}.jpg", "9".repeat(40));
        let longer = format!("page{}.jpg", "1".repeat(41));
        assert_eq!(natural_cmp(&long, &longer), Ordering::Less);
    }

    #[test]
    fn everything_that_is_not_a_digit_compares_byte_wise() {
        assert_eq!(
            sorted(&["b.jpg", "A.jpg", "a.jpg"]),
            ["A.jpg", "a.jpg", "b.jpg"]
        );
        assert_eq!(natural_cmp("page1", "page1extra"), Ordering::Less);
        assert_eq!(natural_cmp("page1", "page1"), Ordering::Equal);
    }

    /// A chapter directory and a cover file are placed by one key rather than grouped.
    #[test]
    fn a_directory_name_sorts_among_the_files_beside_it() {
        assert_eq!(
            sorted(&["chapter2", "cover.jpg", "chapter10", "chapter1"]),
            ["chapter1", "chapter2", "chapter10", "cover.jpg"]
        );
    }
}
