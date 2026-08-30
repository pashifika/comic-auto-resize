#!/usr/bin/env bash
#
# Builds the rar fixtures the test suite cannot build for itself.
#
# Every other archive fixture in this repository is generated at run time by
# `tests/support/mod.rs`. rar cannot be, and the reason is structural rather than
# incidental: UnRAR's licence forbids using its source "to develop RAR (WinRAR) compatible
# archiver and to re-create RAR compression algorithm, which is proprietary", so no open
# implementation exists or lawfully can. RARLAB's `rar` is the only program that writes a
# RAR archive.
#
# So this is a manual, machine-local step. The archiver and the fixtures both land under
# `tools/`, which is gitignored: neither is committed, and CI never runs this. Tests that
# need a fixture skip with a message naming this script when it is absent.
#
# Usage:
#   tests/fixtures/make-rar-fixtures.sh            # fetch rar if needed, then build
#   CAR_RAR_FIXTURES=/somewhere tests/fixtures/make-rar-fixtures.sh
#
# `rar` is proprietary and its download is a trial build. It is used here only to produce
# local test data; nothing it ships is redistributed by this repository.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tools="$root/tools"
out="${CAR_RAR_FIXTURES:-$tools/rar-fixtures}"
page="$root/tests/fixtures/page.jpg"

# 7.23, the release current when this script was written. Pinned rather than floating so a
# fixture that stops reproducing points at a change here rather than at RARLAB's web site.
rar_version="723"

die() { printf '%s: %s\n' "${BASH_SOURCE[0]##*/}" "$1" >&2; exit 1; }
step() { printf '\n== %s\n' "$1"; }

# ---------------------------------------------------------------- the archiver

find_rar() {
    if [ -x "$tools/rar/rar" ]; then printf '%s' "$tools/rar/rar"; return; fi
    if command -v rar >/dev/null 2>&1; then command -v rar; return; fi
    printf ''
}

fetch_rar() {
    case "$(uname -s)-$(uname -m)" in
        Darwin-arm64)  archive="rarmacos-arm-${rar_version}.tar.gz" ;;
        Darwin-x86_64) archive="rarmacos-x64-${rar_version}.tar.gz" ;;
        Linux-x86_64)  archive="rarlinux-x64-${rar_version}.tar.gz" ;;
        Linux-aarch64) archive="rarlinux-arm-${rar_version}.tar.gz" ;;
        *)
            die "no known rar build for $(uname -s)-$(uname -m); install rar yourself and re-run"
            ;;
    esac

    step "fetching rar $rar_version into $tools/rar"
    mkdir -p "$tools"
    curl -fsSL -o "$tools/$archive" "https://www.rarlab.com/rar/$archive"
    tar xzf "$tools/$archive" -C "$tools"
    rm -f "$tools/$archive"
    [ -x "$tools/rar/rar" ] || die "the archive did not contain tools/rar/rar"
}

rar="$(find_rar)"
if [ -z "$rar" ]; then
    fetch_rar
    rar="$tools/rar/rar"
fi
printf 'rar: %s\n' "$rar"
"$rar" 2>&1 | sed -n '2p'

[ -f "$page" ] || die "missing $page"

# ---------------------------------------------------------------- the fixtures

rm -rf "$out"
mkdir -p "$out"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# `rar` refuses to overwrite and writes relative to the current directory, so each fixture
# is built in its own clean subdirectory of a scratch tree.
build_dir() { mkdir -p "$work/$1" && printf '%s' "$work/$1"; }

# Solid and compressed, four pages. The only fixture that reaches the solid dictionary, the
# decompressor, and UnRAR's unpacker threads: both real samples are non-solid and entirely
# stored, so without this the solid path has no evidence at all.
#
# `-s` solid, `-m5` maximum compression, `-ma5` RAR 5.0 format. The pages are byte-identical
# copies under different names, which is deliberate: the first costs its full size and each
# later one is a near-total dictionary match, so the archive only makes sense if the
# dictionary really is shared across entries.
step "solid-compressed.rar"
d="$(build_dir solid)"
for i in 0 1 2 3; do cp "$page" "$d/page0$i.jpg"; done
( cd "$d" && "$rar" a -s -m5 -ma5 -idq "$out/solid-compressed.rar" page0*.jpg )

# A directory entry, which the reader must pass over on its *flag*. Sample B has one; this is
# the small reproduction — plus the case that makes the flag load-bearing.
#
# `cover.jpg` is a directory named as a page. Without the flag check the extension filter
# claims it, the reader tries to read a directory, and the run dies with "named as JPEG but
# its leading bytes are not" — the wrong diagnosis for something that is not a page at all.
# A directory called `pages` cannot show that, because the extension filter skips it anyway.
step "directory-entry.rar"
d="$(build_dir dir)"
mkdir -p "$d/pages" "$d/cover.jpg"
cp "$page" "$d/pages/page00.jpg"
cp "$page" "$d/page01.jpg"
( cd "$d" && "$rar" a -r -m0 -ma5 -idq "$out/directory-entry.rar" pages cover.jpg page01.jpg )

# An entry whose recorded unpacked size exceeds MAX_ENTRY_BYTES (64 MiB), refused before its
# data is read. 64 MiB of zeros compresses to almost nothing, so the header claims a size far
# over the limit while the fixture stays small — which is the shape the check has to catch.
step "oversize-entry.rar"
d="$(build_dir oversize)"
cp "$page" "$d/page00.jpg"
dd if=/dev/zero of="$d/huge.jpg" bs=1048576 count=65 status=none
( cd "$d" && "$rar" a -m5 -ma5 -idq "$out/oversize-entry.rar" page00.jpg huge.jpg )

# The same archive with its entry data cut off, which is what makes "refused *before* its
# data is read" testable rather than merely asserted.
#
# Both entry headers survive the truncation, so the reader reaches `huge.jpg`'s recorded size
# exactly as it would in the intact archive — but there is no longer any data behind it. A
# reader that refuses on the recorded size returns `TooLarge`; one that reads first gets a
# CRC error instead. Two different errors, no timing involved.
step "oversize-truncated.rar"
dd if="$out/oversize-entry.rar" of="$out/oversize-truncated.rar" bs=1 count=1024 status=none

# A multi-volume set, so an entry carries the split flag. The reader refuses it rather than
# following it: a half-followed volume set is a book missing pages.
step "split-entry volumes"
d="$(build_dir split)"
cp "$page" "$d/page00.jpg"
cp "$page" "$d/page01.jpg"
( cd "$d" && "$rar" a -m0 -ma5 -v2k -idq "$out/split-entry.rar" page0*.jpg )

# Four pages whose stored order is not their alphabetical order, because "the reader yields
# them in stored order" is only an assertion if the two differ. `rar` stores in the order the
# names are given.
step "stored-order.rar"
d="$(build_dir order)"
for i in 0 1 2 3; do cp "$page" "$d/page0$i.jpg"; done
( cd "$d" && "$rar" a -m0 -ma5 -idq "$out/stored-order.rar" \
    page02.jpg page00.jpg page03.jpg page01.jpg )

# The shared entry contract, in one archive: an entry no candidate extension claims is passed
# over, and one stored under the encoder's other extension reaches the output renamed.
step "mixed-entries.rar"
d="$(build_dir mixed)"
cp "$page" "$d/page00.jpg"
cp "$page" "$d/page01.jpeg"
printf '<ComicInfo/>' > "$d/notes.xml"
( cd "$d" && "$rar" a -m0 -ma5 -idq "$out/mixed-entries.rar" page00.jpg notes.xml page01.jpeg )

# A stored name longer than the DLL's fixed 1024-wchar field. The dependency used to hand
# back a name cut at 1023 characters with nothing to say it had been, so the page lost its
# extension and was passed over — a page silently missing from the book. Two entries, so a
# reader that drops the long one still looks like it worked.
step "long-name.rar"
d="$(build_dir longname)"
cp "$page" "$d/page00.jpg"
cp "$page" "$d/page01.jpg"
long="$(printf 'a%.0s' $(seq 250))"
( cd "$d" && "$rar" a -m0 -ma5 -idq -ap"$long/$long/$long/$long/$long" \
    "$out/long-name.rar" page00.jpg )
( cd "$d" && "$rar" a -m0 -ma5 -idq "$out/long-name.rar" page01.jpg )

# An over-large entry followed by a good page. One error must end the source: a reader that
# carried on would yield the second page under the index the first would have had.
step "oversize-then-page.rar"
d="$(build_dir oversize2)"
cp "$page" "$d/page01.jpg"
dd if=/dev/zero of="$d/huge.jpg" bs=1048576 count=65 status=none
( cd "$d" && "$rar" a -m5 -ma5 -idq "$out/oversize-then-page.rar" huge.jpg page01.jpg )

# Wrong in two ways at once: a traversing name on an entry that is not a JPEG. The documented
# check order says the content mismatch wins, because a name is only worth refusing on once
# the thing it names is a page.
step "traversing-nonjpeg.rar"
d="$(build_dir tnj)"
printf 'not a jpeg at all' > "$d/page00.jpg"
( cd "$d" && "$rar" a -m0 -ma5 -idq -ap../ "$out/traversing-nonjpeg.rar" page00.jpg )

# Plain headers, encrypted data: the archive reads and one entry does not, so the refusal has
# to name the entry rather than blame the archive.
step "encrypted-data.rar"
d="$(build_dir enc)"
cp "$page" "$d/page00.jpg"
( cd "$d" && "$rar" a -m0 -ma5 -idq -pSecret1 "$out/encrypted-data.rar" page00.jpg )

# An entry whose extension claims a page and whose leading bytes do not. An error rather than
# a skip: the archive is inconsistent, and dropping the page would shorten the book.
step "mismatch-entry.rar"
d="$(build_dir mismatch)"
cp "$page" "$d/page00.jpg"
printf 'this is not a JPEG, whatever the name says' > "$d/page01.jpg"
( cd "$d" && "$rar" a -m0 -ma5 -idq "$out/mismatch-entry.rar" page00.jpg page01.jpg )

# Names that must not be carried into the output archive. `-ap` sets a path prefix inside the
# archive, which is the only way to make `rar` store one of these — it strips them otherwise.
step "unsafe-name archives"
d="$(build_dir unsafe)"
cp "$page" "$d/page00.jpg"
( cd "$d" && "$rar" a -m0 -ma5 -idq -ap../ "$out/traversing-name.rar" page00.jpg )
( cd "$d" && "$rar" a -m0 -ma5 -idq -ap/abs "$out/absolute-name.rar" page00.jpg )

# ---------------------------------------------------------------- independent check
# Change 1's lesson: a fixture must be validated by an implementation that is not the one
# under test, or a failing test cannot be attributed. These are written by RARLAB's own
# archiver rather than by hand, so the format itself is not in doubt — what is checked here
# is that each archive has the shape its test depends on, by a third party.
step "verifying with readers that are neither the writer nor the reader under test"

# Two checkers, doing two different jobs.
#
# libarchive (`bsdtar`) decodes the entry data, so a green result means the bytes are really
# there and their CRCs match — it is a clean-room RAR5 implementation, unrelated to RARLAB's
# `rar` that wrote these and to the UnRAR that `unrar-ng` builds.
#
# 7-Zip reports the structural metadata the fixtures exist for — solid flag, method, entry
# count. It cannot decode RAR 7's compression version 6, which is all `rar` 7.23 writes
# (`-ma4` was removed), so it is deliberately not asked to: `l`, never `t`.
command -v bsdtar >/dev/null 2>&1 || printf 'WARNING: no bsdtar, entry data unverified\n' >&2
if command -v 7zz >/dev/null 2>&1; then seven=7zz
elif command -v 7z >/dev/null 2>&1; then seven=7z
else seven=""; printf 'WARNING: no 7zz/7z, structure unverified\n' >&2
fi

for f in "$out"/*.rar; do
    printf '  %-26s ' "${f##*/}"

    if [ -n "$seven" ]; then
        # Grouped with `|| true` for the same reason as the bsdtar call below: 7-Zip exits
        # non-zero on a continuation volume, which is the correct answer and not a reason to
        # abandon the report.
        { "$seven" l -slt "$f" 2>/dev/null || true; } | awk -F' = ' '
            /^Type =/   { type = $2 }
            /^Solid =/  { solid = $2 }
            /^Method =/ { method = $2 }
            /^Path =/   { n++ }
            END { printf "%-5s solid=%-2s %-14s entries=%d  ", type, solid, method, n - 1 }'
    fi

    if command -v bsdtar >/dev/null 2>&1; then
        case "${f##*/}" in
            # A later volume has no beginning, and the oversize entry is 65 MiB of zeros
            # that there is no reason to materialise. Listed, not extracted.
            split-entry.part[2-9]*|oversize-entry.rar|oversize-then-page.rar)
                bsdtar -tf "$f" >/dev/null 2>&1 \
                    && printf 'data=listed' || printf 'data=UNREADABLE'
                ;;
            *)
                # `|| true` inside the group, because `set -e` with `pipefail` would
                # otherwise abandon the whole loop the first time a reader complains —
                # which is a report this script exists to print, not a reason to stop.
                bytes=$( { bsdtar -xOf "$f" 2>/dev/null || true; } | wc -c | tr -d ' ')
                printf 'data=%s B' "$bytes"
                ;;
        esac
    fi
    printf '\n'
done

step "done"
printf 'fixtures in %s\n' "$out"
ls -la "$out"
