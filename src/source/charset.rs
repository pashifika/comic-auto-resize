//! What characters a stored entry name's bytes name.
//!
//! A stored name is bytes. Which characters they name is the container's to say, and only
//! where it says nothing does this module choose — from a list the user supplied, one
//! encoding for the whole input.
//!
//! Here rather than in `zip.rs` for the reason `unsafe_name`, `is_directory` and `Names` are
//! in `probe`: a rule every format must honour lives where every format reaches it. Only zip
//! calls it today, and that is a property of the other three formats rather than of the rule —
//! rar's decoder discards the stored bytes inside the DLL, 7z stores UTF-16, and a directory's
//! names arrive from the filesystem already decoded.
//!
//! The Go implementation tried its charset list *before* consulting the container's own UTF-8
//! declaration, so a correctly flagged UTF-8 name was decoded as `Shift_JIS` first. That order is
//! a defect and is not inherited; [`Stated`] is the shape that makes inheriting it impossible,
//! because a name the container has settled never reaches an encoding at all.

use encoding_rs::Encoding;
use thiserror::Error;

/// The labels `--charset` defaults to, and the reason it is not `""`.
///
/// The default path is *wrong* for an archive written by a Japanese or Chinese tool — it turns
/// a page into a subdirectory — so the flag defaults to on. Go's default was the same two
/// values; the parity is worth having, but the reason is the measurement rather than the
/// precedent.
pub const DEFAULT_LABELS: &str = "ja,zh";

/// Go's language tags, which are not WHATWG labels.
///
/// `Encoding::for_label` resolves neither `ja` nor `zh` — measured, both return `None` — so the
/// default would be unresolvable without this table. Go accepted exactly these six spellings
/// and mapped them to exactly these three encodings (`utils/config/charset.go:34-46`), and
/// keeping them is what lets the default be spelled the way the reference tool spelled it.
/// Every other value goes to `for_label`, so the full WHATWG label set is accepted too.
static LANGUAGE_TAGS: &[(&str, &str)] = &[
    ("ja", "shift_jis"),
    ("ja-jp", "shift_jis"),
    ("zh", "gb18030"),
    ("zh-cn", "gb18030"),
    ("ko", "euc-kr"),
    ("ko-kr", "euc-kr"),
];

/// The encodings a name with no declared encoding may be decoded from, in the order given.
///
/// Resolved once, when the flag is parsed, so an unknown label is refused before the input is
/// opened. Empty means the reader chooses nothing, which is exactly the behaviour that
/// preceded this type.
#[derive(Clone, Debug, Default)]
pub struct Charset {
    encodings: Vec<&'static Encoding>,
}

/// One entry's name, as far as the container has settled it.
///
/// The distinction is the whole precedence rule: a [`Stated::Decided`] name never reaches an
/// encoding, so no chosen encoding can overwrite what the container declared.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Stated {
    /// Settled already — the container declared an encoding, or its stored bytes are not
    /// reachable and this is the best name there is.
    Decided(String),
    /// The container declared nothing, so an encoding has to be chosen for `bytes`.
    ///
    /// `guess` is the container's own decode of those same bytes — the format's historical
    /// default, which for zip is CP437. It is carried rather than derived because it is the
    /// answer when no encoding is chosen at all, and because only the container can produce
    /// it: the reader has no CP437 table and adding one would be a second source of truth for
    /// the behaviour this type exists to replace.
    Undecided { guess: String, bytes: Vec<u8> },
}

impl Charset {
    /// Resolves a comma-separated label list, in the order given.
    ///
    /// Empty entries are dropped, so `""` resolves to the empty list and `"ja,,zh"` to the
    /// same two encodings as `"ja,zh"`.
    ///
    /// # Errors
    ///
    /// [`UnknownLabel`] naming the label, so the refusal says which value was wrong rather
    /// than that the list was.
    pub fn resolve(labels: &str) -> Result<Self, UnknownLabel> {
        let mut encodings = Vec::new();
        for label in labels.split(',').map(str::trim).filter(|l| !l.is_empty()) {
            let resolved = LANGUAGE_TAGS
                .iter()
                .find(|(tag, _)| tag.eq_ignore_ascii_case(label))
                .map_or(label, |(_, whatwg)| whatwg);
            // `for_label_no_replacement` rather than `for_label`: the label `replacement`
            // resolves to an encoding that fails on every non-empty input, so it could never
            // be chosen and accepting it would only defer the refusal to a run.
            let Some(encoding) = Encoding::for_label_no_replacement(resolved.as_bytes()) else {
                return Err(UnknownLabel {
                    label: label.to_owned(),
                });
            };
            if !encodings.contains(&encoding) {
                encodings.push(encoding);
            }
        }
        Ok(Self { encodings })
    }

    /// The encodings that would be tried, named canonically.
    ///
    /// Canonical names rather than the user's spelling, because a refusal has to say what `ja`
    /// resolved to for the list to be worth printing.
    #[must_use]
    pub fn names(&self) -> String {
        self.encodings
            .iter()
            .map(|encoding| encoding.name())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Every name decoded, choosing at most one encoding for the whole container.
    ///
    /// One encoding for all of them, not one per name: an archive is written by one tool with
    /// one codepage, so a per-name decision could give two pages of one book two different
    /// encodings. The first listed encoding under which **every** undecided name decodes wins.
    ///
    /// A container with nothing undecided needs no encoding, and neither does an empty list —
    /// both leave every name exactly as the container gave it.
    ///
    /// # Errors
    ///
    /// [`Undecodable`] when the list is non-empty, at least one name is undecided, and no
    /// listed encoding decodes all of them. There is no fallback: falling back to the format's
    /// historical default is what this exists to prevent, and a lossy decode would put U+FFFD
    /// in an output name.
    pub fn decode_all(&self, stated: &[Stated]) -> Result<Vec<String>, Undecodable> {
        let undecided = stated
            .iter()
            .any(|name| matches!(name, Stated::Undecided { .. }));
        if !undecided || self.encodings.is_empty() {
            return Ok(stated.iter().map(Stated::as_given).collect());
        }

        // The candidate's decodes are kept as they are made rather than checked and then
        // repeated: the first encoding wins for an ordinary archive, so the common case is one
        // pass over the names.
        for encoding in &self.encodings {
            if let Some(decoded) = decode_every(encoding, stated) {
                return Ok(decoded);
            }
        }
        Err(Undecodable {
            labels: self.names(),
        })
    }
}

/// Every name decoded under `encoding`, or `None` if any undecided name does not decode.
fn decode_every(encoding: &'static Encoding, stated: &[Stated]) -> Option<Vec<String>> {
    let mut decoded = Vec::with_capacity(stated.len());
    for name in stated {
        match name {
            Stated::Decided(name) => decoded.push(name.clone()),
            Stated::Undecided { bytes, .. } => {
                // Neither a BOM nor a replacement character: a name is not a document, so a
                // leading `EF BB BF` is part of it, and a name carrying U+FFFD is a decode
                // that failed rather than one that succeeded lossily.
                let name = encoding.decode_without_bom_handling_and_without_replacement(bytes)?;
                decoded.push(name.into_owned());
            }
        }
    }
    Some(decoded)
}

impl Stated {
    /// The name as the container gave it, with no encoding chosen.
    ///
    /// The answer when nothing is undecided, and the answer an empty encoding list asks for:
    /// the previous behaviour, exactly.
    fn as_given(&self) -> String {
        match self {
            Self::Decided(name) | Self::Undecided { guess: name, .. } => name.clone(),
        }
    }
}

/// A `--charset` value no encoding matches.
#[derive(Debug, Error)]
#[error("unknown encoding label `{label}`")]
pub struct UnknownLabel {
    pub label: String,
}

/// No listed encoding decodes every name the container left undecided.
#[derive(Debug, Error)]
#[error(
    "no name encoding in the list decodes every entry name in this archive (tried {labels}); the archive declares no encoding for them, so pass `--charset` with the one it used, or an empty value to leave the names as the format's historical default"
)]
pub struct Undecodable {
    pub labels: String,
}

#[cfg(test)]
mod tests {
    use super::{Charset, DEFAULT_LABELS, Stated};

    /// `表紙.jpg` in `Shift_JIS`. `95 5C` is the pair that matters: `5C` is the byte for `\`, so
    /// the format's historical default turns this one page into a subdirectory.
    const SJIS_COVER: &[u8] = b"\x95\x5c\x8e\x86.jpg";
    /// `汉字.jpg` in `GB18030` — and *also* valid `Shift_JIS`, where the same four bytes are
    /// halfwidth katakana. The ambiguity is the point: cp437 defines every byte and these two
    /// accept the same name, so "decodes successfully" cannot discriminate and the order the
    /// user gave is what decides.
    const AMBIGUOUS: &[u8] = b"\xba\xba\xd7\xd6.jpg";
    /// `万与.jpg` in `GB18030`, chosen because `Shift_JIS` refuses it: `CD F2` and `D3 EB` are
    /// two-byte `GB18030` characters whose trail bytes fall outside `Shift_JIS`'s range, so the
    /// two encodings genuinely disagree about whether this name exists at all.
    const GB_ONLY: &[u8] = b"\xcd\xf2\xd3\xeb.jpg";
    /// `｡.jpg` in `Shift_JIS`, which `GB18030` refuses: there `A1` is a lead byte and `2E` is not
    /// a valid trail, while `Shift_JIS` reads `A1` as a single halfwidth character.
    const SJIS_ONLY: &[u8] = b"\xa1.jpg";

    /// The container's own decode is not modelled here: these tests are about what a chosen
    /// encoding does, so the guess is a marker that would be obviously wrong if it leaked
    /// into an answer.
    fn undecided(names: &[&[u8]]) -> Vec<Stated> {
        names
            .iter()
            .map(|bytes| Stated::Undecided {
                guess: "<the container's own decode>".to_owned(),
                bytes: bytes.to_vec(),
            })
            .collect()
    }

    /// The default has to resolve, because `Charset::resolve` is the only thing that can say
    /// so and `main` would otherwise fail at parse time on its own default value.
    #[test]
    fn the_default_labels_resolve_to_two_encodings() {
        let charset = Charset::resolve(DEFAULT_LABELS).expect("the default resolves");
        assert_eq!(charset.names(), "Shift_JIS, gb18030");
    }

    /// `ja` and `zh` are language tags, not WHATWG labels — `Encoding::for_label` resolves
    /// neither. This is the measurement the table exists for.
    #[test]
    fn a_language_tag_resolves_where_the_label_set_does_not() {
        for tag in ["ja", "JA", "ja-jp", "zh", "zh-cn", "ko", "ko-kr"] {
            assert!(
                encoding_rs::Encoding::for_label(tag.as_bytes()).is_none(),
                "`{tag}` is a WHATWG label after all, so the table is dead weight"
            );
            assert!(Charset::resolve(tag).is_ok(), "{tag}");
        }
        assert_eq!(
            Charset::resolve("ko").expect("resolves").names(),
            "EUC-KR",
            "the third pair Go accepted"
        );
    }

    /// The full label set is accepted, because restricting it would buy nothing: the crate
    /// cannot be subsetted.
    #[test]
    fn a_whatwg_label_resolves_too() {
        for label in ["shift_jis", "sjis", "windows-31j", "gbk", "big5", "utf-8"] {
            assert!(Charset::resolve(label).is_ok(), "{label}");
        }
    }

    #[test]
    fn an_unknown_label_is_refused_by_name() {
        let error = Charset::resolve("ja,nonsense,zh").expect_err("refused");
        assert_eq!(error.label, "nonsense");
        // `replacement` resolves under `for_label` and can never decode anything, so it is
        // refused at parse time rather than at the first archive.
        assert_eq!(
            Charset::resolve("replacement").expect_err("refused").label,
            "replacement"
        );
    }

    #[test]
    fn an_empty_value_resolves_to_no_encoding() {
        for labels in ["", " ", ",", " , "] {
            let charset = Charset::resolve(labels).expect("resolves");
            assert_eq!(charset.names(), "", "{labels:?}");
        }
    }

    /// The defect this module exists for: the bytes reach the output as the characters they
    /// name, and no part of the name has become a path separator.
    #[test]
    fn a_legacy_name_decodes_to_its_own_characters() {
        let charset = Charset::resolve(DEFAULT_LABELS).expect("resolves");
        let decoded = charset
            .decode_all(&undecided(&[SJIS_COVER]))
            .expect("decodes");
        assert_eq!(decoded, ["表紙.jpg"]);
        assert!(!decoded[0].contains('\\'));
    }

    /// The list is tried in order, and the order is what decides between two encodings that
    /// both accept the bytes.
    #[test]
    fn the_first_listed_encoding_that_decodes_everything_wins() {
        let names = undecided(&[SJIS_COVER]);
        assert_eq!(
            Charset::resolve("ja,zh")
                .expect("resolves")
                .decode_all(&names)
                .expect("decodes"),
            ["表紙.jpg"]
        );
        // The same bytes are valid GB18030, so reversing the list produces a different name.
        // Neither answer is a guess the reader makes on its own: the user chose the order.
        assert_eq!(
            Charset::resolve("zh,ja")
                .expect("resolves")
                .decode_all(&names)
                .expect("decodes"),
            ["昞巻.jpg"]
        );
    }

    /// One encoding for the whole container, not one per name.
    ///
    /// Both these names decode under `Shift_JIS`, so `Shift_JIS` wins for both — and the second
    /// one is a `GB18030` name read as katakana, which is wrong. Deliberately: an archive is
    /// written by one tool with one codepage, and a per-name choice would let two pages of one
    /// book disagree. A container claiming two codepages is a container that lied.
    #[test]
    fn one_encoding_is_chosen_for_the_whole_container() {
        let charset = Charset::resolve("ja,zh").expect("resolves");
        let decoded = charset
            .decode_all(&undecided(&[SJIS_COVER, AMBIGUOUS]))
            .expect("both decode under one encoding");
        assert_eq!(decoded, ["表紙.jpg", "ｺｺﾗﾖ.jpg"]);
    }

    /// And where no single encoding decodes every name, the run is refused rather than split.
    #[test]
    fn a_container_whose_names_need_different_encodings_is_refused() {
        // The premise, asserted rather than assumed: neither listed encoding accepts both
        // names. An encoding-table change that broke this would otherwise leave a test that
        // passes while testing nothing.
        for (encoding, accepted, refused) in [
            (encoding_rs::SHIFT_JIS, SJIS_ONLY, GB_ONLY),
            (encoding_rs::GB18030, GB_ONLY, SJIS_ONLY),
        ] {
            assert!(
                encoding
                    .decode_without_bom_handling_and_without_replacement(accepted)
                    .is_some(),
                "{} must accept {accepted:?}",
                encoding.name()
            );
            assert!(
                encoding
                    .decode_without_bom_handling_and_without_replacement(refused)
                    .is_none(),
                "{} must refuse {refused:?}",
                encoding.name()
            );
        }

        let charset = Charset::resolve("ja,zh").expect("resolves");
        assert!(
            charset
                .decode_all(&undecided(&[SJIS_ONLY, GB_ONLY]))
                .is_err()
        );
    }

    #[test]
    fn a_name_no_listed_encoding_decodes_is_refused() {
        // `FF` is not a lead byte in either listed encoding, and not valid UTF-8.
        let names = undecided(&[b"\xff\xfe\x00page.jpg"]);
        let error = Charset::resolve(DEFAULT_LABELS)
            .expect("resolves")
            .decode_all(&names)
            .expect_err("refused");
        assert_eq!(error.labels, "Shift_JIS, gb18030");
        // No fallback: the refusal is the answer, not a lossy decode.
        assert!(error.to_string().contains("--charset"));
    }

    /// The precedence rule. A name the container settled never reaches an encoding, so the
    /// Go ordering — charset list first, container declaration afterwards — cannot be written
    /// with this type.
    #[test]
    fn a_decided_name_is_not_decoded_again() {
        let charset = Charset::resolve(DEFAULT_LABELS).expect("resolves");
        // Valid Shift_JIS *and* the UTF-8 name the container declared. Decoding it again
        // would produce mojibake from a name that was already right.
        let declared = "汉字测试.jpg".to_owned();
        assert!(
            encoding_rs::SHIFT_JIS
                .decode_without_bom_handling_and_without_replacement(declared.as_bytes())
                .is_none(),
            "pick bytes that would be re-decodable for the test to mean anything"
        );
        let mixed = vec![
            Stated::Decided(declared.clone()),
            Stated::Undecided {
                guess: "<the container's own decode>".to_owned(),
                bytes: SJIS_COVER.to_vec(),
            },
        ];
        assert_eq!(
            charset.decode_all(&mixed).expect("decodes"),
            [declared, "表紙.jpg".to_owned()]
        );
    }

    /// A declared name that does not decode under any listed encoding does not refuse the run:
    /// only undecided names constrain the choice.
    #[test]
    fn a_declared_name_does_not_constrain_the_choice() {
        let charset = Charset::resolve(DEFAULT_LABELS).expect("resolves");
        let mixed = vec![
            Stated::Decided("(一般コミック)/001.jpg".to_owned()),
            Stated::Undecided {
                guess: "<the container's own decode>".to_owned(),
                bytes: SJIS_COVER.to_vec(),
            },
        ];
        assert_eq!(
            charset.decode_all(&mixed).expect("decodes"),
            ["(一般コミック)/001.jpg", "表紙.jpg"]
        );
    }

    /// The empty list reproduces the previous behaviour exactly: whatever the container
    /// decoded, unchanged.
    #[test]
    fn an_empty_list_leaves_every_name_as_the_container_decoded_it() {
        let charset = Charset::resolve("").expect("resolves");
        let mojibake = "ò\\Äå.jpg".to_owned();
        let stated = vec![Stated::Undecided {
            guess: mojibake.clone(),
            bytes: SJIS_COVER.to_vec(),
        }];
        assert_eq!(charset.decode_all(&stated).expect("decodes"), [mojibake]);
    }

    /// A chosen encoding cannot *introduce* a path separator, and this is the check rather
    /// than the claim.
    ///
    /// It matters because `unsafe_name` runs after decoding: if some encoding could turn
    /// harmless bytes into `..` or a `/`, a wrongly chosen one would be a way to smuggle a
    /// traversal past a check performed on the raw bytes. Swept over every byte pair for every
    /// encoding a label in the default list resolves to, plus UTF-16, where a byte pair is one
    /// code unit and the temptation to assume otherwise is strongest.
    ///
    /// The consequence is that decoding only ever *removes* a hazard — `Shift_JIS` consumes `5C`
    /// as the trail byte of `表` — so the after-decoding order is what makes a correct name
    /// acceptable, and a traversal that survives decoding is still refused.
    #[test]
    fn no_encoding_introduces_a_separator_that_was_not_a_byte_of_its_own() {
        const HAZARDS: [char; 4] = ['/', '\\', ':', '\0'];

        // Spelled as WHATWG labels rather than through `Charset::resolve`, because the sweep
        // is about the encodings the default list reaches, not about the aliases that reach
        // them.
        for label in [
            "shift_jis",
            "gb18030",
            "euc-kr",
            "utf-16le",
            "utf-16be",
            "big5",
            "euc-jp",
        ] {
            let encoding = encoding_rs::Encoding::for_label_no_replacement(label.as_bytes())
                .unwrap_or_else(|| panic!("{label} resolves"));
            for first in 0..=u8::MAX {
                for second in 0..=u8::MAX {
                    let raw = [first, second];
                    let Some(decoded) =
                        encoding.decode_without_bom_handling_and_without_replacement(&raw)
                    else {
                        continue;
                    };
                    for hazard in decoded.chars().filter(|c| HAZARDS.contains(c)) {
                        assert!(
                            raw.contains(&(hazard as u8)),
                            "{} turned {raw:02x?} into {decoded:?}, which carries a separator \
                             neither byte was",
                            encoding.name()
                        );
                    }
                }
            }
        }
    }
}
