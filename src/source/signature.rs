//! Which archive format an input holds, decided from a fixed order of candidates.
//!
//! The extension is not consulted, and that is a decision rather than an omission. `.cbz` and
//! `.cbr` are conventions, and the tools that write them get them mixed up; meanwhile this
//! reader already refuses to trust an *entry's* extension over its leading bytes
//! ([`SourceError::Mismatch`](super::SourceError::Mismatch) exists for exactly that).
//! Trusting the archive's extension while distrusting its entries' would be incoherent.
//!
//! A fixed-order slice for the same reason [`probe::CANDIDATES`](super::probe::CANDIDATES) is
//! one: adding a format later must not be able to change how another is found.
//!
//! A directory has no leading bytes and is decided before this module is reached; see
//! [`Source::open`](super::Source::open).

/// An archive format this build can read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveFormat {
    Zip,
    Rar,
    SevenZ,
}

impl ArchiveFormat {
    /// The name used in a diagnostic.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Rar => "rar",
            Self::SevenZ => "7z",
        }
    }
}

/// One format's identifying byte sequences.
pub struct ArchiveCandidate {
    pub format: ArchiveFormat,
    /// Every signature that identifies the format. More than one where the format has
    /// alternatives rather than a single fixed prefix.
    pub magic: &'static [&'static [u8]],
}

/// Every candidate, in the order they are tried.
pub static ARCHIVE_CANDIDATES: &[ArchiveCandidate] = &[
    ArchiveCandidate {
        format: ArchiveFormat::Zip,
        magic: &[
            // A local file header.
            &[b'P', b'K', 3, 4],
            // The end-of-central-directory record an archive with no entries begins with.
            &[b'P', b'K', 5, 6],
        ],
    },
    ArchiveCandidate {
        format: ArchiveFormat::Rar,
        magic: &[
            // RAR 5.0 first: its signature extends RAR 4.x's by one byte, so testing the
            // shorter one first would claim every RAR 5.0 archive as RAR 4.x. Both reach the
            // same reader here, but the order is fixed so that stops being luck.
            &[b'R', b'a', b'r', b'!', 0x1A, 0x07, 0x01, 0x00],
            &[b'R', b'a', b'r', b'!', 0x1A, 0x07, 0x00],
        ],
    },
    ArchiveCandidate {
        format: ArchiveFormat::SevenZ,
        // One fixed prefix, unchanged since the format was published.
        magic: &[&[b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C]],
    },
];

/// How many leading bytes of an input [`detect`] needs.
pub const ARCHIVE_MAGIC_MAX: usize = archive_magic_max();

const fn archive_magic_max() -> usize {
    let mut max = 0;
    let mut candidate = 0;
    while candidate < ARCHIVE_CANDIDATES.len() {
        let magics = ARCHIVE_CANDIDATES[candidate].magic;
        let mut magic = 0;
        while magic < magics.len() {
            if magics[magic].len() > max {
                max = magics[magic].len();
            }
            magic += 1;
        }
        candidate += 1;
    }
    max
}

/// The format `header` begins with, if any.
#[must_use]
pub fn detect(header: &[u8]) -> Option<ArchiveFormat> {
    ARCHIVE_CANDIDATES
        .iter()
        .find(|candidate| {
            candidate
                .magic
                .iter()
                .any(|magic| header.starts_with(magic))
        })
        .map(|candidate| candidate.format)
}

/// The formats this build reads, for a diagnostic that has to name them.
#[must_use]
pub fn readable_formats() -> String {
    ARCHIVE_CANDIDATES
        .iter()
        .map(|candidate| candidate.format.name())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zip_local_header_is_zip() {
        assert_eq!(detect(b"PK\x03\x04rest"), Some(ArchiveFormat::Zip));
    }

    #[test]
    fn an_empty_zip_is_still_zip() {
        assert_eq!(detect(b"PK\x05\x06"), Some(ArchiveFormat::Zip));
    }

    #[test]
    fn a_rar4_signature_is_rar() {
        assert_eq!(detect(b"Rar!\x1a\x07\x00rest"), Some(ArchiveFormat::Rar));
    }

    #[test]
    fn a_rar5_signature_is_rar() {
        assert_eq!(
            detect(b"Rar!\x1a\x07\x01\x00rest"),
            Some(ArchiveFormat::Rar)
        );
    }

    /// The RAR 5.0 signature contains the RAR 4.x one as a prefix, so this is the case that
    /// the candidate order exists to make deterministic.
    #[test]
    fn rar5_is_not_claimed_by_the_shorter_rar4_signature_first() {
        let rar5 = b"Rar!\x1a\x07\x01\x00";
        let matched = ARCHIVE_CANDIDATES
            .iter()
            .flat_map(|candidate| candidate.magic)
            .find(|magic| rar5.starts_with(magic))
            .expect("a signature matches");
        assert_eq!(matched.len(), 8, "the longer signature must be tried first");
    }

    #[test]
    fn a_7z_signature_is_7z() {
        assert_eq!(
            detect(b"7z\xbc\xaf\x27\x1crest"),
            Some(ArchiveFormat::SevenZ)
        );
    }

    #[test]
    fn anything_else_is_nothing() {
        assert_eq!(detect(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(detect(b""), None);
    }

    /// A truncated file must not be claimed on a partial match.
    #[test]
    fn a_prefix_of_a_signature_is_not_a_match() {
        assert_eq!(detect(b"PK"), None);
        assert_eq!(detect(b"Rar!"), None);
        assert_eq!(detect(b"7z\xbc\xaf"), None);
    }

    #[test]
    fn the_magic_window_covers_the_longest_signature() {
        assert_eq!(ARCHIVE_MAGIC_MAX, 8);
    }

    #[test]
    fn the_diagnostic_names_every_format() {
        assert_eq!(readable_formats(), "zip, rar, 7z");
    }
}
