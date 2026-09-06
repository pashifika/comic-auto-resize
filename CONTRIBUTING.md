# Contributing

Read [CLAUDE.md](CLAUDE.md) before changing architecture, dependency policy, or the
resize strategy. It is the baseline this repository is verified against; this file
covers building it and getting a change merged.

The Rust implementation is developed on `dev/2.0.x` and promoted to `main` at parity.
See [`README.md`](README.md) to build and run the tool.

## Toolchain and lockfile

`rust-toolchain.toml` pins the compiler together with `rustfmt` and `clippy`, so a clean
checkout with `rustup` available selects the tested toolchain automatically. CI reads the
channel back out of that same file rather than repeating the number, so bumping the pin
is a one-line change.

That pin is not the crate's minimum supported Rust version. `Cargo.toml`'s `rust-version`
is, and it is deliberately lower: the pin says what is tested, the manifest says what is
required.

`Cargo.lock` is committed. Run every check with `--locked` so a stale lockfile fails
loudly instead of being silently updated, and commit the lockfile change in the same pull
request as the manifest change that caused it.

## Prerequisites

`rustup`, a C compiler, and `nasm` on x86.

The image pipeline links mozjpeg, and `mozjpeg-sys` builds it from source on every clean
build. That needs a C compiler on every target, and `nasm` on x86 for the SIMD kernels —
on aarch64 the kernels are assembled by the C compiler itself, so `nasm` is not used
there. Without the assembler it needs, `mozjpeg-sys` compiles a scalar fallback and says
so only in a `cargo:warning`, so CI asserts that `WITH_SIMD` survived rather than trusting
the build to be loud about it.

```sh
# macOS
brew install nasm      # only needed on an Intel Mac

# Windows
choco install nasm     # then add C:\Program Files\NASM to PATH
```

The `mozjpeg` crate is patched to a Git revision until its upstream pull request lands;
see the comment beside `[patch.crates-io]` in `Cargo.toml`. If your global Git
configuration rewrites `https://github.com/` to SSH — `url.<ssh>.insteadOf` — Cargo's
bundled fetcher will try ssh-agent and fail with `no authentication methods succeeded`.
Either add the key to `ssh-agent` or run Cargo with `CARGO_NET_GIT_FETCH_WITH_CLI=true`.

`cargo-deny` is needed for the dependency-policy step below:

```sh
cargo install --locked cargo-deny --version 0.20.2
```

### rar fixtures, if you are touching the rar reader

Not needed to build, and not needed for the verification sequence below: the rar tests that
depend on these fixtures skip when they are absent, and CI never builds them.

They are a separate step because rar is the one format this repository cannot write for
itself. UnRAR's licence forbids using its source "to develop RAR (WinRAR) compatible archiver
and to re-create RAR compression algorithm, which is proprietary", so no open implementation
exists or lawfully can, and RARLAB's `rar` is the only program that writes a RAR archive.

```sh
tests/fixtures/make-rar-fixtures.sh
```

It fetches `rar` into `tools/` when it is not already there or on `PATH`, writes four fixtures
to `tools/rar-fixtures/`, and checks each one with `bsdtar` and `7zz` — readers that are
neither the writer nor the reader under test. `tools/` is gitignored: the archiver is
proprietary and the fixtures are derived from it, so neither is committed.

What the fixtures are for is the part worth knowing. Both real rar samples are non-solid and
entirely stored, so they exercise header walking and the stored reader and nothing else. The
solid, compressed fixture is the only evidence that the shared dictionary, the decompressor,
and UnRAR's unpacker threads work at all.

Set `CAR_RAR_FIXTURES` to write them somewhere other than `tools/rar-fixtures/`.

### 7z fixtures, if you are touching the 7z reader

Nothing to run. Unlike rar, 7z has an open writer, so `tests/sevenz_source.rs` builds every
fixture it needs at test time — but it needs a 7-Zip command-line archiver to do it, and skips
with a message naming what to install when there is none:

```sh
brew install sevenzip      # macOS, provides `7zz`
choco install 7zip         # Windows, provides `7z`
```

Both names are tried, in that order. Both runner images already ship one — 7-Zip 26.02 on
`windows-2025`, p7zip 17.05 on `macos-15` — and CI asserts it is there rather than trusting
the image, because a suite that skipped every test is not a suite that passed.

Worth knowing what the fixtures stand in for: `samples/` holds two zips and two rars and no 7z
at all, so unlike rar — where two real archives caught a dropped page no synthetic fixture
would have — every 7z claim in this repository rests on an archive `7zz` wrote.

### BMP fixtures, if you are touching the image decoders

Not needed to build, and not needed for the verification sequence below: the tests that depend
on these skip when they are absent, and CI never builds them.

BMP is not one format but a family — `BITMAPCOREHEADER` through `BITMAPV5HEADER`, seven
compression schemes, 1 to 64 bits per pixel, top-down and bottom-up, palettes that may be
absent or offset. Hand-rolling fixtures for that would be writing a second BMP implementation
to test the first with, so this fetches Jason Summers' BMP Suite and runs its own generator:

```sh
tests/fixtures/make-bmp-fixtures.sh
```

It needs `git`, `make` and a C compiler, all of which `mozjpeg-sys` already requires. Output
lands in `tools/bmp-fixtures/` — 89 files across `g/` (must read), `q/` (this project decides,
per file), `b/` (must refuse without crashing) and `x/` (must not be mistaken for BMP) — and
`tools/` is gitignored.

Two things about it are load-bearing rather than incidental. The generator is **GPL-3.0**, and
this project's allow-list carries no GPL term, so the script *runs* it and redistributes
nothing — the same distinction that lets `make-rar-fixtures.sh` use RARLAB's proprietary
archiver. And the generated images are public domain by the author's explicit statement
*except* for two that embed an ICC profile, so the script deletes those two and then proves the
exclusion is complete by grepping the corpus for the profile signature rather than trusting the
list.

Set `CAR_BMP_FIXTURES` to write them somewhere other than `tools/bmp-fixtures/`.

## Building

```sh
cargo build --locked --release
```

The binary lands at `target/release/comic-auto-resize` (`comic-auto-resize.exe` on Windows).
Run it with `--help` for options or follow the examples in [`README.md`](README.md).

## Verification

Run this sequence from the repository root before opening a pull request. It is ordered
so the cheapest structural failure is reported first, and every step exits non-zero with
an actionable diagnostic when its policy is violated.

```sh
# 1. Formatting. Instant, and the most common reason a pull request goes red.
cargo fmt --all --check

# 2. The branch-flow rules, verified before they are trusted to judge a pull request.
python3 -B .github/scripts/test_branch_policy.py

# 3. Dependency policy: licences, advisories, duplicate versions, and sources.
#    Resolves metadata only, so it costs nothing to run early.
cargo deny --locked check

# 4. Lints, with warnings promoted to failures.
cargo clippy --locked --all-targets --all-features -- -D warnings

# 5. Tests.
cargo test --locked --all-features

# 6. The release profile, which enables LTO and can fail where a debug build does not.
cargo build --locked --release
```

CI runs step 1 through 3 once, on Linux, and steps 4 through 6 natively on both release
targets. Neither target is cross-compiled: a cross-compiled result would not be evidence
that the shipped binary works.

If you have [`actionlint`](https://github.com/rhysd/actionlint) installed, run it after
editing the workflow. CI does not, so a syntax error there costs a round trip.

## Branch flow and pull requests

```
feat|fix|perf|refactor|docs|test|build|ci|chore|revert/<slug>
        │
        ▼
   dev/2.0.x ◀── main   synchronize after each promotion
        │
        ▼  once, at parity with master
      main
```

Cut a short-lived topic branch from `dev/2.0.x`, using one of the prefixes above with a
non-empty slug. Topic branches merge into `dev/2.0.x`. A topic branch targeting `main`
directly is rejected by the branch-flow check, and so is a pull request into `main` from a
fork.

`dev/2.0.x` merges into `main` once, at parity with `master`. `main` requires its head to
be up to date with itself, so each promotion leaves `dev/2.0.x` one merge commit behind.
Catch it up by opening a pull request from `main` into `dev/2.0.x`. That direction is the
one exception to topic-branch-only heads, and it is the only way to do it: the development
ruleset requires a pull request and the `ci` check, so a direct fast-forward push is
refused with `GH013`.

Both branches are protected and require the `ci` status check. `main` accepts merge
commits only and requires every review thread resolved; `dev/2.0.x` is looser and accepts
any merge method. `master` cannot be deleted or force-pushed; it is a reference branch
and new work does not belong on it.

Use [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) for commit
subjects.

## Continuous integration

Two workflows. `.github/workflows/ci.yml` defines `hygiene` and `release-validation` on
Linux, `windows` and `macos` on their respective release targets, the two shell-completion
jobs, and a terminal `ci` job that fails unless every one of them reported `success` — a
*skipped* job is not a passing job, so the gate checks each result by name rather than
calling `success()`. `.github/workflows/release.yml` builds and publishes downloads; it is
never a required pull-request status, and only its offline half runs in `ci`.

`ci` is the only status context the branch rulesets require, and they name no individual
job. **Adding a job therefore means adding it to the `ci` gate's `needs` list and nothing
else.** No repository setting changes, and removing a job later cannot strand a required
check that will never report again.

Every `uses:` reference must be pinned to a full 40-character commit SHA with a trailing
comment naming the version:

```yaml
uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
```

`hygiene` enforces this across the whole workflow tree, and fails if it finds no
references at all — a check that verified nothing is not a passing check.

The branch rulesets are declared under `.github/rulesets/`, together with the version-tag
ruleset. Those files are the source of truth, but committing one does not enforce it; a
maintainer applies them to the repository through `gh api`.

## Releasing

Stable binary releases are published from an existing `vMAJOR.MINOR.PATCH` tag whose commit
is contained in `main`. The workflow never creates, moves, or deletes a tag, and it refuses
to publish unless the repository default branch is `main`, `ci` succeeded for that exact
commit on `main`, and the tagged tree's `.github/workflows` matches current `main` — the
Actions token cannot write workflow files, and a divergent tree is where that surfaces.

### What a release produces

```text
comic-auto-resize-<tag>-x86_64-pc-windows-msvc.zip
comic-auto-resize-<tag>-aarch64-apple-darwin.tar.gz
SHA256SUMS
```

Each archive has one top-level directory matching its filename stem, holding the executable,
`LICENSE`, `NOTICE.md`, a generated `THIRD-PARTY-LICENSES.txt`, and `VERSION`. The licence
bundle is built from the locked non-development dependency graph for that target, including
text a crate carries somewhere other than its own root: `unrar-ng-sys` vendors RARLAB's
modified UnRAR and ships no root licence file, and packaging fails rather than dropping it.

Repackaging the same inputs is byte-deterministic. Independent builds on a changed runner
image are not claimed to be, which is what the published SHA-256 values are for.

### Prepare a version

Bump `version` in `Cargo.toml` through the normal topic-to-development flow, let Cargo
rewrite the root entry in `Cargo.lock`, and confirm no dependency resolution changed. Run
the verification sequence above, promote `dev/2.0.x` to `main`, and record the resulting
`main` commit. Never tag a topic or development commit.

### Rehearse without publishing

`workflow_dispatch` exercises both native builds, the licence bundle, the conversion smoke,
packaging, and aggregation, then stops. The workflow must already exist on the default
branch for the dispatch to be offered:

```sh
gh workflow run Release --ref main -f ref=<commit>
gh run list --workflow Release --event workflow_dispatch --limit 1
gh run watch <run-id> --exit-status
```

Both `build` rows and `aggregate` must succeed, and `publish` and `verify-published` must be
skipped. Manual preflight always reports `publish=false`, and the publish job independently
requires a tag-push event, so a rehearsal that selected a real release tag still cannot
publish.

### Protect version tags, once

Apply `.github/rulesets/version-tags.json` only after `release.yml` is present on `main`,
and check first that no tag ruleset already exists:

```sh
gh api --method GET repos/{owner}/{repo}/rulesets -f targets=tag
gh api repos/{owner}/{repo}/rulesets --input .github/rulesets/version-tags.json
```

Read the result back and require the expected name, `tag` target, `active` enforcement,
exact `refs/tags/v*` include with no exclusions, and an empty bypass list. GitHub's readback
normalizes the `update` rule by omitting its parameters; the reviewed request payload is the
evidence for `update_allows_fetch_and_merge=false`. GitHub documents an effective-rules
endpoint for branches only, so do not test the rules by moving or deleting a real tag — read
the non-destructive rule-suite evaluation for the ref after the tag is created normally.

### Create the tag

```sh
git tag -a <tag> <commit> -m "comic-auto-resize <tag>"
git push origin refs/tags/<tag>
```

The push is the public-release authorization boundary: nothing before it is visible, and
nothing after it can be taken back. Do not merge workflow changes between creating the tag
and publication.

### Verify the published release

The two `verify-published` jobs already download the public assets, check the checksums and
package identity, extract, and run a real conversion natively. Repeat it by hand from an
empty directory when a release is being accepted:

```sh
gh release download <tag> --dir <empty-directory>
python3 -B .github/scripts/release.py verify-set \
  --directory <empty-directory> --version <MAJOR.MINOR.PATCH> --tag <tag> --commit <commit>
```

### Failure and retry

| Failure | Required outcome |
|---|---|
| Preflight, build, smoke, package or aggregate failure | No Release interaction happened. Fix forward; if the tag exists, use a new patch version rather than moving it. |
| Workflow tree differs from current `main`, or main CI is not green for the commit | Stop before Release interaction. Promote the intended state and release a new version. |
| Upload interrupted after the draft was created | A marker-bound draft remains. Rerun the same tag's workflow while its artifacts are retained. |
| Draft missing an asset, or holding an `open` incomplete one | Only that asset is uploaded or replaced, then the complete set is redownloaded and verified. |
| Uploaded asset, marker, tag commit, title or asset set conflicts | Stop for review. Never clobber, never publish the draft. |
| Retry after a successful publication | The existing set is downloaded and verified; nothing is mutated. |
| A published binary is defective | Increment the patch version and release a new tag. Never rewrite published bytes. |

### Evidence to retain

Preparation and promotion pull requests, the promoted `main` commit, the tag object, the
workflow run and attempt, runner and `rustc -vV` evidence, the archive SHA-256 values, the
release ID and asset listing, the tag-ruleset payload and readback, and the native
downloaded-asset results. Signing, notarization, crates.io, and package-manager channels are
not provided and are not implied by any of the above.
