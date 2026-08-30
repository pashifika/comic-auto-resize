//! Which image format an entry holds, decided from a fixed order of candidates.
//!
//! Go iterated `decoders`, a Go map, so the order two formats were tried in varied between
//! runs (`utils/images/images.go:74`). This is a slice in a stated order. JPEG is the only
//! entry today; the slice exists so that adding png cannot change how jpeg is found.

use crate::page::SOI_MARKER;

/// An image format this build can decode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    Jpeg,
}

impl Format {
    /// The extension the encoder writes for this format, without the dot.
    ///
    /// Every page leaves as JPEG, so this is also the extension every output entry carries.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Jpeg => "JPEG",
        }
    }
}

/// One format's identifying bytes and the extensions it is stored under.
pub struct Candidate {
    pub format: Format,
    /// The bytes an entry of this format begins with.
    ///
    /// The single source of truth: [`probe`] compares exactly these, and [`MAGIC_MAX`] is
    /// derived from them, so a candidate whose declared length and compared bytes disagree
    /// cannot be written. That is the defect at `utils/images/plugs/bmp.go`, where
    /// `Matched` compares the webp header while `HeaderLen` returns the bmp header's
    /// length, making the candidate permanently unmatchable.
    pub magic: &'static [u8],
    /// Lower-case, without the dot. Matched case-insensitively.
    pub extensions: &'static [&'static str],
}

/// Every candidate, in the order they are tried.
pub static CANDIDATES: &[Candidate] = &[Candidate {
    format: Format::Jpeg,
    magic: &SOI_MARKER,
    extensions: &["jpg", "jpeg"],
}];

/// How many leading bytes of an entry [`probe`] needs.
///
/// The longest candidate's magic, so an entry that is not an image costs a header read
/// rather than a decode attempt.
pub const MAGIC_MAX: usize = magic_max();

const fn magic_max() -> usize {
    let mut max = 0;
    let mut index = 0;
    while index < CANDIDATES.len() {
        if CANDIDATES[index].magic.len() > max {
            max = CANDIDATES[index].magic.len();
        }
        index += 1;
    }
    max
}

/// The format `header` begins with, if any.
#[must_use]
pub fn probe(header: &[u8]) -> Option<Format> {
    CANDIDATES
        .iter()
        .find(|candidate| header.starts_with(candidate.magic))
        .map(|candidate| candidate.format)
}

/// The format `name`'s extension claims, if any.
///
/// Consulted before the bytes are read, as Go did: it filtered the archive walk by
/// extension and only then compared magic bytes, so a `ComicInfo.xml` never reached a
/// decoder and a `page01.jpg` that was not a JPEG was an error rather than a skip.
#[must_use]
pub fn declared_format(name: &str) -> Option<Format> {
    let extension = split_extension(name).1?;
    CANDIDATES
        .iter()
        .find(|candidate| {
            candidate
                .extensions
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        })
        .map(|candidate| candidate.format)
}

/// `name` with its extension replaced by `format`'s.
///
/// Go renamed every entry to the encoder's extension (`utils/archiver/compressor.go:39`,
/// `af.RenameExt`), so `page01.jpeg` becomes `page01.jpg`. The extension describes the
/// bytes, and the bytes are always JPEG here. Everything before the extension, directories
/// included, is carried through untouched.
#[must_use]
pub fn output_name(name: &str, format: Format) -> String {
    let (stem, _) = split_extension(name);
    let mut renamed = String::with_capacity(stem.len() + 1 + format.extension().len());
    renamed.push_str(stem);
    renamed.push('.');
    renamed.push_str(format.extension());
    renamed
}

/// Splits `name` into everything before its extension and the extension itself.
///
/// The separator search comes first, so a dot in a directory name is not read as the
/// extension of a file that has none. A leading dot is a hidden name rather than an
/// extension, matching how `Path::extension` treats it.
fn split_extension(name: &str) -> (&str, Option<&str>) {
    let start = name.rfind(['/', '\\']).map_or(0, |separator| separator + 1);
    match name[start..].rfind('.') {
        Some(0) | None => (name, None),
        Some(dot) => (&name[..start + dot], Some(&name[start + dot + 1..])),
    }
}

#[cfg(test)]
mod tests {
    use super::{CANDIDATES, Format, MAGIC_MAX, declared_format, output_name, probe};

    #[test]
    fn the_compared_bytes_are_the_bytes_the_candidate_declares() {
        // The `bmp.go` defect made unrepresentable: each candidate is matched by its own
        // `magic`, so a candidate cannot be permanently unmatchable.
        for candidate in CANDIDATES {
            assert!(
                !candidate.magic.is_empty(),
                "{:?} declares no magic bytes",
                candidate.format
            );
            assert!(candidate.magic.len() <= MAGIC_MAX);

            let mut header = candidate.magic.to_vec();
            header.extend_from_slice(&[0; 16]);
            assert_eq!(
                probe(&header),
                Some(candidate.format),
                "{:?} does not match its own magic bytes",
                candidate.format
            );

            // Flip the last declared byte: no candidate may still claim it.
            let mut wrong = header.clone();
            let last = candidate.magic.len() - 1;
            wrong[last] = !wrong[last];
            assert_ne!(
                probe(&wrong),
                Some(candidate.format),
                "{:?} matches bytes it does not declare",
                candidate.format
            );
        }
    }

    #[test]
    fn the_probe_reads_no_more_than_the_longest_magic() {
        assert_eq!(MAGIC_MAX, 2, "JPEG's FF D8 is the only candidate today");
        // A prefix shorter than the magic cannot match, so a truncated header is not a
        // false positive.
        assert_eq!(probe(&[0xFF]), None);
        assert_eq!(probe(&[]), None);
    }

    #[test]
    fn a_non_image_header_matches_nothing() {
        assert_eq!(probe(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(probe(b"<?xml version"), None);
    }

    #[test]
    fn the_extension_decides_which_entries_are_pages() {
        assert_eq!(declared_format("page01.jpg"), Some(Format::Jpeg));
        assert_eq!(declared_format("page01.jpeg"), Some(Format::Jpeg));
        assert_eq!(declared_format("page01.JPG"), Some(Format::Jpeg));
        assert_eq!(declared_format("pages/page01.Jpeg"), Some(Format::Jpeg));

        assert_eq!(declared_format("ComicInfo.xml"), None);
        assert_eq!(declared_format("Thumbs.db"), None);
        assert_eq!(declared_format("README"), None);
        // A dot in a directory name is not the file's extension.
        assert_eq!(declared_format("v1.2/cover"), None);
        // A leading dot is a hidden name.
        assert_eq!(declared_format(".jpg"), None);
    }

    #[test]
    fn the_stem_survives_and_only_the_extension_changes() {
        assert_eq!(
            output_name("pages/page01.jpg", Format::Jpeg),
            "pages/page01.jpg"
        );
        assert_eq!(output_name("page01.jpeg", Format::Jpeg), "page01.jpg");
        assert_eq!(output_name("page01.JPEG", Format::Jpeg), "page01.jpg");
        // A directory with a dot keeps it.
        assert_eq!(
            output_name("v1.2/page01.jpeg", Format::Jpeg),
            "v1.2/page01.jpg"
        );
        // A backslash separator, as a Windows-written archive may use.
        assert_eq!(
            output_name("pages\\page01.jpeg", Format::Jpeg),
            "pages\\page01.jpg"
        );
    }
}
