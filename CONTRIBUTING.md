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

Only `rustup`. The skeleton depends on `clap` and nothing native.

This will change: the image pipeline links mozjpeg, which `mozjpeg-sys` builds from
source and which needs a C compiler and `nasm` for its SIMD paths. That prerequisite
arrives with the change that adds the dependency, and this section is updated then rather
than in advance.

`cargo-deny` is needed for the dependency-policy step below:

```sh
cargo install --locked cargo-deny --version 0.20.2
```

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
        ▼
      main
```

Cut a short-lived topic branch from `dev/2.0.x`, using one of the prefixes above with a
non-empty slug. Topic branches merge into `dev/2.0.x`; `dev/2.0.x` merges into `main`. A
topic branch targeting `main` directly is rejected by the branch-flow check, and so is a
pull request into `main` from a fork.

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
