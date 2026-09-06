# Notices

Obligations carried by this project's dependencies that `cargo deny` cannot enforce.

`deny.toml` reads crate metadata and, for licence text, only files whose names begin with
`LICENSE` or `COPYING` at a crate's own root — it does not recurse. A dependency that vendors
third-party source therefore passes on the strength of its own manifest, whatever its tree
contains. This file exists for that gap, and `tests/notice.rs` asserts it has not been tidied
away.

## UnRAR, via `unrar-ng` / `unrar-ng-sys`

This project reads RAR archives through `unrar-ng`, which builds RARLAB's UnRAR from C++ sources
vendored at `unrar_sys/vendor/unrar/`. The vendored tree is **UnRAR 7.21 beta 1** (2026-03-22, per
`vendor/unrar/version.hpp`).

Dependency chain:

| | |
|---|---|
| Direct dependency | `unrar-ng` 0.7.7, pinned by revision |
| Consumed from | `https://github.com/pashifika/unrar.rs` |
| Which forks | `https://github.com/ttys3/unrar.rs` |
| Which forks | `https://github.com/muja/unrar.rs` |
| Vendored component | UnRAR, © Alexander L. Roshal |

### Why this entry exists

`cargo deny` reports no finding against either crate. Both declare `MIT OR Apache-2.0`, and
`unrar-ng-sys` ships no `LICENSE*` or `COPYING*` file at its crate root at all, so the UnRAR
licence at `vendor/unrar/license.txt` is invisible to the check. A green dependency-policy run
therefore means "the metadata is acceptable", which is a narrower claim than a reader takes from
it.

### The source is modified

The vendored UnRAR is not RARLAB's tarball unaltered. `unrar_sys/vendor/patches/` records seven
patches applied on top of it:

| Patch | What it changes |
|---|---|
| `0001-fix-rar-open-archive-ex-bad-data-handle.patch` | handle returned for a bad-data archive |
| `0002-fix-readheader-thread-safe-error.patch` | thread-safety of the header-read error path |
| `0003-chore-guard-builtin-cpu-supports.patch` | guards `__builtin_cpu_supports` |
| `0004-feat-rar-extract-all-batch.patch` | batch extract-all entry point |
| `0005-perf-rar-extract-all-w-loop.patch` | batch extract-all loop |
| `0006-feat-ucm-extractfile-callbacks.patch` | `UCM_EXTRACTFILE` callback events |
| `0007-fix-linux-widetochar-use-utf8.patch` | locale-independent wide/8-bit filename conversion |

Earlier releases of the sys crate recorded this in a single `vendor/patches.txt`; that file was
removed when the crate moved from cherry-picks to `git apply`, and the directory above replaced it.
Both are excluded from the crates.io package, which is one reason this project consumes the crate
from Git rather than from the registry.

Because the vendored source is modified, UnRAR's clause 2 attaches to whoever distributes it. That
is directly true of the fork above, and it reaches this project through the binaries built from it,
so the paragraph is reproduced here in full as clause 2 requires.

### UnRAR licence, clause 2, reproduced in full

> UnRAR source code may be used in any software to handle
> RAR archives without limitations free of charge, but cannot be
> used to develop RAR (WinRAR) compatible archiver and to
> re-create RAR compression algorithm, which is proprietary.
> Distribution of modified UnRAR source code in separate form
> or as a part of other software is permitted, provided that
> full text of this paragraph, starting from "UnRAR source code"
> words, is included in license, or in documentation if license
> is not available, and in source code comments of resulting package.

The full UnRAR licence is at `unrar_sys/vendor/unrar/license.txt` in the dependency's tree.

### Maintenance

This entry is refreshed whenever the vendored UnRAR version or the patch series changes. Bumping
the pinned revision in `[patch.crates-io]` without checking both is how it goes stale.
