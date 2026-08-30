//! Which image format an entry holds, decided from a fixed order of candidates.
//!
//! Go iterated `decoders`, a Go map, so the order two formats were tried in varied between
//! runs (`utils/images/images.go:74`). This is a slice in a stated order. JPEG is the only
//! entry today; the slice exists so that adding png cannot change how jpeg is found.

use std::collections::HashMap;
use std::fmt::Write as _;

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

/// How an output entry's name is derived from the name the input stored.
///
/// The default keeps Change 1's rule — a stored name reaches the output with only its
/// extension rewritten — and the other is reachable only because the user asked for it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Naming {
    #[default]
    Stored,
    /// Each page's trailing digit run replaced by the page's own position, so a viewer that
    /// orders entries by name shows the book in its right order.
    ByPosition,
}

/// Derives every output entry name for one run.
///
/// One type rather than a rule copied into four readers, for the reason `unsafe_name` and
/// `is_directory` moved up in Change 2: a property that must hold for every format belongs
/// where every format reaches it.
///
/// The entry total is taken at construction because [`Naming::ByPosition`] needs a width and
/// the pipeline is streaming — the first page's name is decided long before the last page is
/// seen. Constructed per variant rather than from a total plus a flag, so a format whose
/// total costs a second pass over the input pays for it only when it is used.
#[derive(Debug)]
pub struct Names {
    positions: Option<Positions>,
}

/// The state [`Naming::ByPosition`] needs: one width, and one counter per directory.
#[derive(Debug)]
struct Positions {
    /// `digits(total)`, which is the smallest width at which ordering by name agrees with
    /// ordering by number.
    width: usize,
    /// Directory component of the entry name to the next position within it. A flat archive
    /// holds exactly one entry, keyed by the empty string.
    next: HashMap<String, u32>,
}

impl Names {
    /// Names carry through with only their extension rewritten.
    #[must_use]
    pub fn stored() -> Self {
        Self { positions: None }
    }

    /// Names carry their own position, at the width `total` entries require.
    ///
    /// `total` is the entry total rather than a count of pages. An exact page count would
    /// make the counting pass duplicate the extension filter to move a digit in a book of
    /// exactly 100 candidate pages holding one entry that is not a page, so the width is
    /// allowed to be one wider than strictly needed.
    #[must_use]
    pub fn by_position(total: usize) -> Self {
        Self {
            positions: Some(Positions {
                width: digits(total),
                next: HashMap::new(),
            }),
        }
    }

    /// The output name for `stored`, advancing the position counter when it renames.
    pub fn of(&mut self, stored: &str, format: Format) -> String {
        match &mut self.positions {
            None => output_name(stored, format),
            Some(positions) => positions.of(stored, format),
        }
    }
}

impl Positions {
    fn of(&mut self, stored: &str, format: Format) -> String {
        let (stem, _) = split_extension(stored);
        let prefix = stem.trim_end_matches(|character: char| character.is_ascii_digit());
        // No trailing digit run, so there is nothing for the rule to replace and it does
        // not run. Not a case carved out of the rule: `cover.jpg` keeps its name, and the
        // counter does not advance, so an unnumbered entry consumes no page number.
        if prefix.len() == stem.len() {
            return output_name(stored, format);
        }
        // Per directory *component of the entry name*, whatever the input's kind: an archive
        // stores `/` in an entry name as readily as a filesystem does, so `ch1/` and `ch2/`
        // are two runs of pages either way.
        //
        // The key folds `\` to `/`, and that is what makes the collision-freedom claim true
        // of *paths* rather than only of strings. An archive written on Windows stores `\`,
        // so `ch1/page5.jpg` and `ch1\page9.jpg` are two spellings of one directory: two
        // buckets would give both position one, two distinct zip entry names, and one file
        // once extracted on Windows — with the writer's duplicate-name refusal comparing
        // exact strings and so unable to fire. One bucket, one sequence, no collision. The
        // output name keeps the separator the input used; only the counter is shared.
        let directory = &prefix[..prefix.rfind(['/', '\\']).map_or(0, |at| at + 1)];
        let position = self
            .next
            .entry(directory.replace('\\', "/"))
            .and_modify(|next| *next += 1)
            .or_insert(1);

        let extension = format.extension();
        let mut renamed =
            String::with_capacity(prefix.len() + 1 + self.width + 1 + extension.len());
        renamed.push_str(prefix);
        // Omitted where the file part of the prefix is empty, so `1.jpg` becomes `001.jpg`,
        // and where it does not end in an alphanumeric, so `page_1.jpg` does not gain a
        // second underscore.
        if prefix.len() > directory.len()
            && prefix
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
        {
            renamed.push('_');
        }
        // The position rather than the number the input recorded, which is what makes the
        // rule statable without deciding which digits in a name were meant to be the page.
        write!(renamed, "{position:0width$}", width = self.width)
            .expect("writing to a String cannot fail");
        renamed.push('.');
        renamed.push_str(extension);
        renamed
    }
}

/// How many decimal digits `total` occupies, and never fewer than one.
fn digits(total: usize) -> usize {
    let mut digits = 1;
    let mut remaining = total / 10;
    while remaining > 0 {
        digits += 1;
        remaining /= 10;
    }
    digits
}

#[cfg(test)]
mod tests {
    use super::{
        CANDIDATES, Format, MAGIC_MAX, Names, declared_format, digits, output_name, probe,
    };

    /// Every name a `Names` produces for `stored`, in order.
    fn renamed(total: usize, stored: &[&str]) -> Vec<String> {
        let mut names = Names::by_position(total);
        stored
            .iter()
            .map(|name| names.of(name, Format::Jpeg))
            .collect()
    }

    #[test]
    fn the_width_is_what_the_entry_total_requires() {
        assert_eq!(digits(0), 1);
        assert_eq!(digits(9), 1);
        assert_eq!(digits(10), 2);
        assert_eq!(digits(95), 2);
        assert_eq!(digits(99), 2);
        assert_eq!(digits(100), 3);
        assert_eq!(digits(210), 3);
    }

    #[test]
    fn a_page_is_renamed_to_its_own_position_not_the_number_it_recorded() {
        assert_eq!(
            renamed(95, &["page7.jpg", "page8.jpg", "page9.jpg"]),
            ["page_01.jpg", "page_02.jpg", "page_03.jpg"]
        );
    }

    #[test]
    fn a_name_with_no_trailing_digit_run_is_left_alone_and_consumes_no_position() {
        assert_eq!(
            renamed(3, &["cover.jpg", "page1.jpg", "page2.jpg"]),
            ["cover.jpg", "page_1.jpg", "page_2.jpg"]
        );
    }

    #[test]
    fn digits_that_are_not_a_page_number_are_not_interpreted() {
        assert_eq!(renamed(10, &["page1of10.jpg"]), ["page1of_01.jpg"]);
    }

    #[test]
    fn a_separator_is_not_doubled_and_a_bare_number_gains_none() {
        assert_eq!(renamed(9, &["page_1.jpg"]), ["page_1.jpg"]);
        assert_eq!(renamed(9, &["1.jpg"]), ["1.jpg"]);
        assert_eq!(renamed(9, &["page-4.jpg"]), ["page-1.jpg"]);
        assert_eq!(renamed(9, &["ch1/7.jpeg"]), ["ch1/1.jpg"]);
    }

    /// Per *directory*, and a directory is the same directory however its separator is
    /// spelled: `ch2/` and `ch2\` share one counter, so the two entries take positions one
    /// and two rather than both taking one and colliding as a path on extraction. The output
    /// keeps the separator each name arrived with; only the sequence is shared.
    #[test]
    fn each_directory_component_restarts_the_count_at_one_width() {
        assert_eq!(
            renamed(
                29,
                &[
                    "ch1/page1.jpg",
                    "ch1/page2.jpg",
                    "ch2/page1.jpg",
                    "ch2\\page2.jpg",
                ]
            ),
            [
                "ch1/page_01.jpg",
                "ch1/page_02.jpg",
                "ch2/page_01.jpg",
                "ch2\\page_02.jpg",
            ]
        );
    }

    /// A padding rule would have collapsed these two onto `page01.jpg`. The positional rule
    /// cannot: one counter per directory makes every position within it distinct.
    #[test]
    fn two_names_a_padding_rule_would_have_collapsed_stay_distinct() {
        let names = renamed(2, &["page1.jpg", "page_1.jpg"]);
        assert_eq!(names, ["page_1.jpg", "page_2.jpg"]);
        assert_ne!(names[0], names[1]);
    }

    /// The three halves of the collision-freedom claim, on one mixed input: a name left
    /// alone ends in a non-digit, every renamed name ends in a digit, and no two entries
    /// arrive at one name.
    #[test]
    fn a_renamed_name_and_a_name_left_alone_cannot_collide() {
        let stored = [
            "cover.jpg",
            "page1.jpg",
            "1.jpg",
            "page1of10.jpg",
            "x_2.jpg",
        ];
        let names = renamed(5, &stored);
        // One counter for the whole (empty) directory component, so `cover.jpg` is the only
        // entry that does not take a position and every other entry takes the next one.
        assert_eq!(
            names,
            [
                "cover.jpg",
                "page_1.jpg",
                "2.jpg",
                "page1of_3.jpg",
                "x_4.jpg"
            ]
        );

        for name in &names {
            let stem = super::split_extension(name).0;
            let numbered = stem.ends_with(|character: char| character.is_ascii_digit());
            assert_eq!(numbered, name != "cover.jpg", "{name}");
        }

        let distinct: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(distinct.len(), names.len(), "{names:?}");
    }

    #[test]
    fn the_default_naming_rewrites_only_the_extension() {
        let mut names = Names::stored();
        assert_eq!(names.of("page1.jpeg", Format::Jpeg), "page1.jpg");
        assert_eq!(names.of("ch1/cover.JPG", Format::Jpeg), "ch1/cover.jpg");
    }

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
