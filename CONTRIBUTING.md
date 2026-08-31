# Contributing

Read [CLAUDE.md](CLAUDE.md) before changing architecture, dependency policy, or the
resize strategy. It is the baseline this repository is verified against; this file
covers building it and getting a change merged.

`main` is a rewrite in progress. If you are looking for the tool that currently works,
it is on `master`.

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
cargo build --release
```

The binary lands at `target/release/comic-auto-resize`. Today it prints its version and
exits.

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
   dev/2.0.x
        │
        ▼  once, at parity with master
      main
```

Cut a short-lived topic branch from `dev/2.0.x`, using one of the prefixes above with a
non-empty slug. Topic branches merge into `dev/2.0.x`. A topic branch targeting `main`
directly is rejected by the branch-flow check, and so is a pull request into `main` from a
fork.

`dev/2.0.x` merges into `main` once, at parity with `master`. `main` requires its head to
be up to date with itself, so each promotion leaves `dev/2.0.x` one merge commit behind;
fast-forward `dev/2.0.x` onto `main` afterwards.

Both branches are protected and require the `ci` status check. `main` accepts merge
commits only and requires every review thread resolved; `dev/2.0.x` is looser and accepts
any merge method. `master` cannot be deleted or force-pushed; it is a reference branch
and new work does not belong on it.

Use [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) for commit
subjects.

## Continuous integration

One workflow, `.github/workflows/ci.yml`. It defines `hygiene` on Linux, `windows` and
`macos` on their respective release targets, and a terminal `ci` job that fails unless
every one of them reported `success` — a *skipped* job is not a passing job, so the gate
checks each result by name rather than calling `success()`.

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

The three branch rulesets are declared under `.github/rulesets/`. Those files are the
source of truth, but committing one does not enforce it; a maintainer applies them to the
repository through `gh api`.
