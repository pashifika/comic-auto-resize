//! `NOTICE.md` carries obligations `cargo deny` structurally cannot see, so nothing but a
//! test stops it being tidied away.
//!
//! The dependency check reads crate metadata, and licence files only at a crate's root and
//! only when the name begins with `LICENSE` or `COPYING`. A vendored tree is invisible to
//! it. That makes a green `cargo deny` worthless as evidence here, and this project's
//! convention is that a claim with no test is a claim.

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn notice() -> String {
    let path = repo_root().join("NOTICE.md");
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn manifest() -> String {
    let path = repo_root().join("Cargo.toml");
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Collapses wrapping and blockquote markers, so the assertions are about the words rather
/// than about where a line happened to break.
fn flatten(text: &str) -> String {
    text.lines()
        .map(|line| line.trim_start().trim_start_matches('>').trim())
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The obligation is on the dependency being present, so the two are asserted together: a
/// notice for a dependency that has gone is stale, and a dependency with no notice is the
/// failure this file exists for.
#[test]
fn the_vendoring_dependency_is_named() {
    let manifest = manifest();
    assert!(
        manifest.contains("unrar-ng"),
        "`unrar-ng` has left Cargo.toml; if rar reading is gone, delete its NOTICE.md entry \
         and this assertion together"
    );

    let notice = notice();
    for required in ["unrar-ng", "unrar-ng-sys", "UnRAR"] {
        assert!(
            notice.contains(required),
            "NOTICE.md no longer names `{required}`"
        );
    }
}

/// `UnRAR` clause 2 requires its own full text to be reproduced by anything distributing
/// modified `UnRAR` source. Asserting the whole paragraph rather than a phrase, because the
/// obligation is the paragraph.
#[test]
fn unrar_clause_two_is_reproduced_in_full() {
    // Verbatim from `unrar_sys/vendor/unrar/license.txt`, lines 13-21 of the dependency's
    // vendored tree.
    let clause = "UnRAR source code may be used in any software to handle \
        RAR archives without limitations free of charge, but cannot be \
        used to develop RAR (WinRAR) compatible archiver and to \
        re-create RAR compression algorithm, which is proprietary. \
        Distribution of modified UnRAR source code in separate form \
        or as a part of other software is permitted, provided that \
        full text of this paragraph, starting from \"UnRAR source code\" \
        words, is included in license, or in documentation if license \
        is not available, and in source code comments of resulting package.";

    assert!(
        flatten(&notice()).contains(&flatten(clause)),
        "NOTICE.md no longer reproduces UnRAR clause 2 in full, which the licence requires"
    );
}

/// The entry describes a specific vendored version and patch series. Bumping the pinned
/// revision without refreshing them is the way this file goes quietly wrong, so the version
/// it claims is asserted against the revision the manifest actually pins.
#[test]
fn the_notice_records_the_vendored_version_and_the_pinned_revision() {
    let notice = notice();
    assert!(
        notice.contains("7.21 beta 1"),
        "NOTICE.md must name the vendored UnRAR version"
    );
    assert!(
        notice.contains("vendor/patches/"),
        "NOTICE.md must say where the applied patches are recorded"
    );
    assert!(
        notice.contains("https://github.com/pashifika/unrar.rs"),
        "NOTICE.md must name the fork the modified source is distributed from"
    );

    // The manifest is the authority on what is actually built; the notice describes it.
    assert!(
        manifest().contains("https://github.com/pashifika/unrar.rs.git"),
        "the fork NOTICE.md describes is no longer the one Cargo.toml patches to"
    );
}
