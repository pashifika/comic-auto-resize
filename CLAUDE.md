# Repository Guidelines

`CONTRIBUTING.md` defines the toolchain, prerequisites, verification sequence, branch,
and pull-request policy, and `README.md` states what the tool is and which branch to
use. Treat the rules in this file as the default project guidance for everything else,
and do not restate what those two already say: two copies of a rule are two rules.

## Product definition

comic-auto-resize opens a compressed comic archive, shrinks every page, and writes a new
archive, holding pages in memory rather than unpacking to disk. Its purpose is reducing
archive size; page quality is what the reduction must not destroy.

Input formats are zip, rar, 7z, and a plain directory. The output format is zip, always,
regardless of input. Release targets are `x86_64-pc-windows-msvc` and
`aarch64-apple-darwin`.

The tool does not own a GUI, a daemon, a watch mode, or any network access. Output
formats other than zip, and a reusable archive-abstraction library, are out of scope: the
Go implementation factored the latter out and then had to maintain a fork of the standard
zip reader to keep it.

## Repository layout and branches

`master` holds the shipped Go implementation, v1.1.2, and is frozen — every write to it is
rejected, and it remains the repository default branch until the rewrite reaches parity.
`main` holds the Rust rewrite. `dev/2.0.x` is the integration line feeding `main`.

The behavioural reference the rewrite is measured against is the `master` branch itself.
A convenience checkout of it may be present at `examples/comic-auto-resize-master/`; that
path is ignored and machine-local, so read it if it is there and fall back to
`git show master:<path>` if it is not.

## Size reduction strategy

Reduction comes from pixel count, not from quantisation. Lowering JPEG quality produces
ringing and mosquito noise on the high-contrast line art that dominates manga, while
reducing pixel count reduces bytes roughly with area and lets the resampler keep lines
clean. Resize is therefore the primary lever and encoder quality stays high.

mozjpeg is used for both encoding and decoding. Its value is the encoder — trellis
quantisation, progressive scan optimisation, and tuned quantisation tables produce a
smaller file at equal quality — and its decoder is used because scaled decode and IDCT
method selection are needed and no pure-Rust decoder offers them.

## Architecture

The pipeline is a single streaming pass with no look-ahead: a sequential reader feeds a
bounded channel, worker threads decode, resize, and encode, and an ordering writer
reassembles entries into the output archive. Peak memory is a function of worker count,
not of page count. Reading is sequential because solid rar and 7z archives cannot be
accessed randomly.

Archive sources are an enum rather than trait objects. Image format probing uses a
fixed-order static slice so the probe order is deterministic. Organize modules by
responsibility; do not introduce a general `utils` layer.

This crate declares `unsafe_code = "forbid"`. FFI stays inside dependencies.

## Dependencies and licensing

`cargo deny` enforces the licence allow-list, and a dependency whose licence is outside it
needs an explicit exception carrying a comment that explains why. A dependency that
vendors third-party source under a licence its own metadata does not name — `unrar` is the
case that matters here — additionally needs a `NOTICE.md` entry, because `cargo deny`
reads metadata and cannot see vendored trees.

Pin dependency versions exactly. A Git dependency needs a pinned revision and an
`allow-git` entry, and it is a temporary measure: record what has to happen upstream
before it can be removed.

## Testing

Add tests for changed behaviour, and prefer tests that would fail on a plausible bug over
tests that restate the implementation. Output is not bit-identical to the Go version —
the resampler and the mozjpeg release both differ — so compare behaviour and observable
properties rather than bytes.

Both release targets are verified natively; neither is cross-compiled, because a
cross-compiled result is not evidence that the shipped binary works. The commands and
their ordering are in `CONTRIBUTING.md`.

## Continuous integration

The branch rulesets require one status context, `ci`, and name no individual job, so
adding a job means extending the `ci` gate's `needs` list and nothing else — never a
repository setting. Every `uses:` reference is pinned to a full 40-character commit SHA
with a trailing version comment, enforced by the `hygiene` job.
