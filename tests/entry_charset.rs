//! `--charset` and `--pwd`: what characters a stored entry name's bytes name, and what
//! happens to an entry the archive encrypted.
//!
//! Every fixture here is assembled byte by byte, for a reason the corpus cannot fix. Two
//! `samples/` zips do reach the defect — one holds 218 `Shift_JIS` names with the UTF-8 flag
//! clear, and its names are valid `GB18030` as well, so it exercises the ordering rule too — but
//! `samples/` is machine-local and 850 MB, so CI never sees it. These fixtures are what runs
//! everywhere, and each one stands in for exactly one rule.
//!
//! `ZipWriter` cannot write any of them. It takes a name as `&str`, encodes UTF-8, and sets
//! general-purpose bit 11 for anything non-ASCII — the opposite of the archive under test. The
//! one exception is the `ZipCrypto` fixture, where the cipher has to be the real one and
//! `zip`'s own writer produces it with no feature change.

mod support;

use std::process::Command;

use comic_auto_resize::page::{DecodeSettings, EncodeSettings, Filter};
use comic_auto_resize::pipeline::{self, Settings};
use comic_auto_resize::policy::AUTO_WIDTH;
use comic_auto_resize::sink::{InputKind, default_output};
use comic_auto_resize::source::{Charset, ReadOptions, Source, SourceError, ZipSource};
use support::{Encoded, Encryption, TempDir, encoded_archive, page_bytes, read_archive};

const BINARY: &str = env!("CARGO_BIN_EXE_comic-auto-resize");

/// `表紙.jpg` in `Shift_JIS`. `95 5C` is the pair the whole Change turns on: `5C` is the byte for
/// `\`, so CP437 — what the format says to use when bit 11 is clear — makes `ò\Äå.jpg` of it and
/// one page becomes a subdirectory.
const SJIS_COVER: &[u8] = b"\x95\x5c\x8e\x86.jpg";
/// `頁001.jpg` in `Shift_JIS`.
const SJIS_PAGE: &[u8] = b"\x95\xc5001.jpg";
/// `汉字.jpg` in `GB18030` — and also valid `Shift_JIS`, where the same four bytes are halfwidth
/// katakana. Ambiguity is the ordinary case, not the exception: cp437 defines every byte and
/// these two encodings accept the same name, so nothing but the user's order can choose.
const AMBIGUOUS: &[u8] = b"\xba\xba\xd7\xd6.jpg";
/// `万与.jpg` in `GB18030`, which `Shift_JIS` refuses: `CD F2` and `D3 EB` are two-byte `GB18030`
/// characters whose trail bytes fall outside `Shift_JIS`'s range.
const GB_ONLY: &[u8] = b"\xcd\xf2\xd3\xeb.jpg";
/// `｡.jpg` in `Shift_JIS`, which `GB18030` refuses: there `A1` is a lead byte and `2E` is not a
/// valid trail.
const SJIS_ONLY: &[u8] = b"\xa1.jpg";

fn page() -> Vec<u8> {
    page_bytes(64, 90)
}

fn settings() -> Settings {
    Settings {
        jobs: std::num::NonZeroUsize::new(2).expect("two"),
        target_width: AUTO_WIDTH,
        filter: Filter::default(),
        decode: DecodeSettings::default(),
        encode: EncodeSettings::default(),
    }
}

fn options(labels: &str) -> ReadOptions {
    ReadOptions {
        charset: Charset::resolve(labels).expect("the label list resolves"),
        ..Default::default()
    }
}

/// Every output name the reader produces for `bytes` under `options`.
fn names(bytes: &[u8], options: &ReadOptions) -> Result<Vec<String>, SourceError> {
    let mut source = ZipSource::new(std::io::Cursor::new(bytes.to_vec()), options)?;
    let mut names = Vec::new();
    while let Some(entry) = source.next_entry() {
        names.push(entry?.name);
    }
    Ok(names)
}

/// The defect, and the fixture the corpus could not supply until it did: a legacy-encoded name
/// with the UTF-8 flag clear reaches the output as its own characters, and nothing in it has
/// become a path separator.
#[test]
fn a_shift_jis_name_reaches_the_output_as_its_own_characters() {
    let archive = encoded_archive(&[
        Encoded::new(SJIS_COVER, page()),
        Encoded::new(SJIS_PAGE, page()),
    ]);

    assert_eq!(
        names(&archive, &options("ja,zh")).expect("reads"),
        ["表紙.jpg", "頁001.jpg"]
    );

    // The previous behaviour, which the empty list restores exactly. Kept as an assertion
    // rather than as a claim: it is what makes the line above a fix.
    let mojibake = names(&archive, &ReadOptions::default()).expect("reads");
    assert_eq!(mojibake, ["ò\\Äå.jpg", "ò┼001.jpg"]);
    assert!(
        mojibake[0].contains('\\'),
        "the defect is a path separator, not mojibake: {mojibake:?}"
    );
}

/// The list is tried in order, and the order is what decides between two encodings that both
/// accept the bytes. Ambiguity is the ordinary case here, not a crafted one: cp437 defines
/// every byte, so "decodes successfully" cannot discriminate on its own — which is why the
/// flag takes an ordered list rather than a detector.
#[test]
fn the_first_listed_encoding_that_decodes_every_name_is_used() {
    let archive = encoded_archive(&[Encoded::new(SJIS_COVER, page())]);

    assert_eq!(
        names(&archive, &options("ja,zh")).expect("reads"),
        ["表紙.jpg"]
    );
    assert_eq!(
        names(&archive, &options("zh,ja")).expect("reads"),
        ["昞巻.jpg"]
    );
}

/// A `GB18030` name is reached only because the list names it: `Shift_JIS` is tried first and
/// refuses these bytes, so the second entry in the list is what decodes the archive.
#[test]
fn a_gb18030_name_is_decoded_by_the_second_encoding_in_the_list() {
    let archive = encoded_archive(&[Encoded::new(GB_ONLY, page())]);

    assert_eq!(
        names(&archive, &options("ja,zh")).expect("reads"),
        ["万与.jpg"]
    );
    // Shift_JIS alone cannot decode them, and the refusal is the answer rather than a
    // fallback to a lossy decode or to CP437.
    let error = names(&archive, &options("ja")).expect_err("refused");
    assert!(
        matches!(error, SourceError::Charset(_)),
        "expected a charset refusal, got {error}"
    );
}

/// One encoding for the whole container, not one per name.
///
/// `AMBIGUOUS` decodes under both listed encodings, so `Shift_JIS` wins it too and reads it as
/// katakana rather than as the Chinese name it was written as — the trade the all-or-nothing
/// rule makes deliberately, because an archive is written by one tool with one codepage and a
/// per-entry decision would let two pages of one book disagree.
#[test]
fn one_encoding_is_chosen_for_every_name_in_the_container() {
    let archive = encoded_archive(&[
        Encoded::new(SJIS_COVER, page()),
        Encoded::new(AMBIGUOUS, page()),
    ]);

    assert_eq!(
        names(&archive, &options("ja,zh")).expect("reads"),
        ["表紙.jpg", "ｺｺﾗﾖ.jpg"]
    );
    // And under the reversed list, both are Chinese.
    assert_eq!(
        names(&archive, &options("zh,ja")).expect("reads"),
        ["昞巻.jpg", "汉字.jpg"]
    );
}

/// And where no single encoding decodes every name, the run is refused rather than split.
///
/// `SJIS_ONLY` is not valid `GB18030` and `GB_ONLY` is not valid `Shift_JIS`, so neither listed
/// encoding can take the whole container.
#[test]
fn a_container_whose_names_disagree_is_refused_rather_than_split() {
    let archive = encoded_archive(&[
        Encoded::new(SJIS_ONLY, page()),
        Encoded::new(GB_ONLY, page()),
    ]);

    let error = names(&archive, &options("ja,zh")).expect_err("refused");
    let message = error.to_string();
    assert!(
        message.contains("Shift_JIS") && message.contains("gb18030"),
        "the refusal must name the encodings tried: {message}"
    );
    assert!(
        message.contains("--charset"),
        "the refusal must say how to answer it: {message}"
    );
}

/// No listed encoding decodes the names, and the run is refused rather than falling back.
#[test]
fn a_name_no_listed_encoding_decodes_is_refused() {
    // `FF FE` leads nothing in either listed encoding, and is not valid UTF-8 either.
    let archive = encoded_archive(&[Encoded::new(b"\xff\xfe\x01page.jpg", page())]);

    assert!(matches!(
        names(&archive, &options("ja,zh")).expect_err("refused"),
        SourceError::Charset(_)
    ));
}

/// The container's own declaration outranks the chosen encoding.
///
/// This is the fixture that fails if the ordering is Go's: its names are valid `Shift_JIS` *and*
/// declared UTF-8, so trying the charset list first — which Go did, before consulting the flag
/// at all — produces mojibake from a name that was already right.
#[test]
fn a_flagged_utf8_name_is_not_decoded_again() {
    let name = "汉字测试.jpg";
    assert!(
        encoding_rs::GB18030
            .decode_without_bom_handling_and_without_replacement(name.as_bytes())
            .is_some(),
        "the fixture only means something if its UTF-8 bytes are also valid in a listed encoding"
    );
    let archive = encoded_archive(&[Encoded::new(name.as_bytes(), page()).utf8()]);

    for labels in ["ja,zh", "zh,ja", "zh", ""] {
        assert_eq!(
            names(&archive, &options(labels)).expect("reads"),
            [name],
            "--charset {labels}"
        );
    }
}

/// An Info-ZIP Unicode Path field outranks both the flag and the chosen encoding.
///
/// The dependency applies the field itself and overwrites the raw name with its UTF-8 content,
/// which is why `name_raw()` is "the best name bytes the crate has" rather than "the bytes in
/// the central directory" — and why a decoder that assumed the latter would re-decode an
/// already correct name through `Shift_JIS`. The field's characters disagree with the stored
/// name deliberately, so the assertion distinguishes the two.
#[test]
fn a_stated_unicode_name_wins_over_the_stored_bytes() {
    let archive = encoded_archive(&[Encoded::new(SJIS_COVER, page()).unicode_path("表紙絵.jpg")]);

    for labels in ["ja,zh", ""] {
        assert_eq!(
            names(&archive, &options(labels)).expect("reads"),
            ["表紙絵.jpg"],
            "--charset {labels}"
        );
    }
}

/// A declared name and an undeclared one in one archive: the declared one is believed and does
/// not constrain the choice, and the undeclared one is decoded.
#[test]
fn a_declared_name_does_not_decide_the_encoding_for_an_undeclared_one() {
    let archive = encoded_archive(&[
        // Valid UTF-8, declared, and *not* valid Shift_JIS — so requiring it to decode under
        // the chosen encoding would refuse this archive.
        Encoded::new("日本語.jpg".as_bytes(), page()).utf8(),
        Encoded::new(SJIS_COVER, page()),
    ]);

    assert_eq!(
        names(&archive, &options("ja,zh")).expect("reads"),
        ["日本語.jpg", "表紙.jpg"]
    );
}

/// The safety check runs on the *decoded* name, which is the name that reaches the output.
///
/// A traversal survives decoding, so it is refused — and the refusal names the decoded form
/// rather than the bytes, which is how the ordering is observable at all. The opposite
/// direction is unreachable rather than untested: no WHATWG multi-byte encoding emits `/`,
/// `\`, `:` or NUL for an input byte that was not already that value, so a chosen encoding
/// cannot *introduce* a separator. That is asserted over every byte pair in
/// `src/source/charset.rs`, where the encodings are.
#[test]
fn a_traversing_legacy_name_is_refused_by_its_decoded_name() {
    // `../表紙.jpg` stored as Shift_JIS.
    let archive = encoded_archive(&[Encoded::new(b"../\x95\x5c\x8e\x86.jpg", page())]);

    match names(&archive, &options("ja")).expect_err("refused") {
        SourceError::UnsafeName { name, reason } => {
            assert_eq!(name, "../表紙.jpg");
            assert!(reason.contains("escapes"), "{reason}");
        }
        other => panic!("expected a traversal refusal, got {other}"),
    }
}

/// Two stored names that differ byte for byte and decode onto one name both survive the entry
/// table, because the table is keyed on the raw bytes — and decoding is what makes them
/// collide. The writer's duplicate refusal is what catches it.
#[test]
fn two_names_that_decode_onto_one_are_refused_by_the_writer() {
    // The undeclared name decodes to exactly the string the declared one already is, so the
    // two entries differ in the table — which is keyed on the raw bytes — and collide only
    // after decoding. That is why the reader relies on the writer's refusal here rather than
    // trying to keep decoded names distinct itself.
    let archive = encoded_archive(&[
        Encoded::new("表紙.jpg".as_bytes(), page()).utf8(),
        Encoded::new(SJIS_COVER, page()),
    ]);
    let scratch = TempDir::new("charset-collision");
    let input = scratch.join("in.zip");
    std::fs::write(&input, &archive).expect("writes the fixture");

    // The reader hands on both, under one name.
    assert_eq!(
        names(&archive, &options("ja")).expect("reads"),
        ["表紙.jpg", "表紙.jpg"]
    );

    let output = scratch.join("out.zip");
    let source = Source::open(&input, &options("ja")).expect("opens");
    let error = pipeline::run(source, &output, &settings()).expect_err("a collision");
    assert!(
        matches!(error, pipeline::RunError::NameCollision { .. }),
        "expected the writer's duplicate refusal, got {error}"
    );
}

/// A non-page entry is passed over on the decoded name, and an entry whose codec or encryption
/// would make locating it fail does not stop a run that never wanted it.
///
/// This is the property the alternative implementation — locate every entry, then filter —
/// would have given up: `by_index` on an encrypted `ComicInfo.xml` returns an error, so a run
/// would fail on an entry the filter was about to drop.
#[test]
fn a_non_page_entry_is_passed_over_even_when_it_could_not_be_read() {
    let archive = encoded_archive(&[
        Encoded::new(SJIS_COVER, page()),
        Encoded::new(b"ComicInfo.xml", b"<ComicInfo/>".to_vec()).encrypted(Encryption::Aes256),
    ]);

    assert_eq!(
        names(&archive, &options("ja")).expect("reads"),
        ["表紙.jpg"]
    );
}

/// An encrypted entry with no password is refused by name, and its data is not read.
#[test]
fn an_encrypted_entry_with_no_password_is_refused() {
    let archive =
        encoded_archive(&[Encoded::new(b"page01.jpg", page()).encrypted(Encryption::ZipCrypto)]);

    let error = names(&archive, &ReadOptions::default()).expect_err("refused");
    assert!(
        matches!(error, SourceError::Encrypted { ref name } if name == "page01.jpg"),
        "expected a named encryption refusal, got {error}"
    );
    assert!(error.to_string().contains("--pwd"), "{error}");
}

/// An AES entry is refused by form, distinguished from an unsupported codec and from a wrong
/// password.
///
/// Both with and without a password, because `AexEncryption::parse` rewrites the compression
/// method to the underlying one — so an AES entry opens like any other, and the dependency's
/// own answers are `InvalidPassword` with no password and "cannot be decrypted without the
/// aes-crypto feature" with one. Neither says which form it is.
#[test]
fn an_aes_entry_is_refused_by_form() {
    let archive =
        encoded_archive(&[Encoded::new(b"page01.jpg", page()).encrypted(Encryption::Aes256)]);

    for password in [None, Some("hunter2".to_owned())] {
        let options = ReadOptions {
            password,
            ..Default::default()
        };
        let error = names(&archive, &options).expect_err("refused");
        match error {
            SourceError::EncryptionUnsupported { ref name, form } => {
                assert_eq!(name, "page01.jpg");
                assert_eq!(form, "AES-256");
            }
            other => panic!("expected an AES refusal by form, got {other}"),
        }
    }
}

/// A `ZipCrypto` entry with the right password is read and processed like any other page.
///
/// The one fixture whose cipher has to be real, so `zip`'s own writer produces it — which is
/// itself the measurement that `ZipCrypto` needs no feature: `mod zipcrypto` is unconditional
/// and this build's manifest names no encryption feature at all.
#[test]
fn a_zipcrypto_entry_with_the_right_password_is_read() {
    let scratch = TempDir::new("charset-zipcrypto");
    let input = scratch.join("in.zip");
    support::write_encrypted_archive(
        &input,
        &[("page01.jpg", page_bytes(320, 440))],
        "correct horse",
    );

    let options = ReadOptions {
        password: Some("correct horse".to_owned()),
        ..Default::default()
    };
    let output = scratch.join("out.zip");
    let source = Source::open(&input, &options).expect("opens");
    let report = pipeline::run(source, &output, &settings()).expect("runs");
    assert_eq!(report.pages, 1);
    assert_eq!(read_archive(&output).len(), 1);

    // And without the password the same archive is refused rather than read as plaintext.
    let error = names(
        &std::fs::read(&input).expect("reads the fixture"),
        &ReadOptions::default(),
    )
    .expect_err("refused");
    assert!(matches!(error, SourceError::Encrypted { .. }), "{error}");
}

/// A wrong password says a wrong password is a possible cause, because on this encryption form
/// it is: `ZipCrypto` authenticates against one byte, so one wrong password in 256 is accepted
/// and what surfaces afterwards is a page that will not decode.
#[test]
fn a_wrong_password_names_itself_as_a_possible_cause() {
    let scratch = TempDir::new("charset-badpassword");
    let input = scratch.join("in.zip");
    support::write_encrypted_archive(&input, &[("page01.jpg", page_bytes(320, 440))], "right");

    let options = ReadOptions {
        password: Some("wrong".to_owned()),
        ..Default::default()
    };
    let error = names(&std::fs::read(&input).expect("reads"), &options).expect_err("refused");
    let message = error.to_string();
    assert!(
        message.contains("password"),
        "the refusal must name the password: {message}"
    );
}

/// The two new flags reach the output, which is the `command-line` requirement every accepted
/// flag has to satisfy.
#[test]
fn the_charset_flag_changes_the_output_and_an_unknown_label_is_refused() {
    let scratch = TempDir::new("charset-cli");
    let input = scratch.join("in.zip");
    std::fs::write(
        &input,
        encoded_archive(&[Encoded::new(SJIS_COVER, page_bytes(320, 440))]),
    )
    .expect("writes the fixture");

    // The default decodes it, so the default run is the fixed one.
    let status = Command::new(BINARY)
        .arg(&input)
        .status()
        .expect("runs the binary");
    assert!(status.success());
    let output = default_output(&input, InputKind::File).expect("names an output");
    assert_eq!(
        read_archive(&output)
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        ["表紙.jpg"]
    );
    std::fs::remove_file(&output).expect("removes the output");

    // An empty list turns the decoding off, which is a different output for the same input.
    let status = Command::new(BINARY)
        .args(["--charset", ""])
        .arg(&input)
        .status()
        .expect("runs the binary");
    assert!(status.success());
    assert_eq!(
        read_archive(&output)
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        ["ò\\Äå.jpg"]
    );
    std::fs::remove_file(&output).expect("removes the output");

    // An unknown label is refused by the parser, so no output is created at all.
    let refused = Command::new(BINARY)
        .args(["--charset", "ja,nonsense"])
        .arg(&input)
        .output()
        .expect("runs the binary");
    assert!(!refused.status.success());
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(message.contains("nonsense"), "{message}");
    assert!(!output.exists(), "the input was opened despite a bad label");
}

/// `--pwd` requires a value, and its help names what this build can decrypt.
#[test]
fn the_password_flag_requires_a_value_and_its_help_names_the_forms() {
    let scratch = TempDir::new("charset-pwd-cli");
    let input = scratch.join("in.zip");
    std::fs::write(
        &input,
        encoded_archive(&[Encoded::new(b"page01.jpg", page_bytes(320, 440))]),
    )
    .expect("writes the fixture");

    let refused = Command::new(BINARY)
        .arg("--pwd")
        .output()
        .expect("runs the binary");
    assert!(!refused.status.success());

    let help = Command::new(BINARY)
        .arg("--help")
        .output()
        .expect("runs the binary");
    let text = String::from_utf8_lossy(&help.stdout);
    let pwd = text
        .split("--pwd")
        .nth(1)
        .expect("--pwd is listed")
        .split("\n\n")
        .next()
        .expect("a description");
    for named in ["ZipCrypto", "rar", "AES"] {
        assert!(pwd.contains(named), "--pwd's help must name {named}: {pwd}");
    }

    let charset = text
        .split("--charset")
        .nth(1)
        .expect("--charset is listed")
        .split("\n\n")
        .next()
        .expect("a description");
    assert!(
        charset.contains("declares none") || charset.contains("declares no"),
        "--charset's help must say what it applies to: {charset}"
    );
    assert!(
        charset.contains("whole input"),
        "--charset's help must say one encoding is chosen for the whole input: {charset}"
    );
}
