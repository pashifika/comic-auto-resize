comic-auto-resize
=================

A manga archive auto-resize tool: it opens a compressed comic file, shrinks every page,
and writes a new zip — all in memory, without unpacking to disk.

## Status

**This branch is a Rust rewrite in progress and does not work yet.**

| branch | contents |
|---|---|
| `master` | The shipped Go implementation, v1.1.2. Frozen; it is the reference the rewrite is measured against. |
| `main` | The Rust rewrite. Carries the JPEG page codec as a library; no working CLI yet. |
| `dev/2.0.x` | Integration line for the rewrite. |

`master` remains the repository default branch until the rewrite reaches feature parity.
Until then, use `master` — see its [README](https://github.com/pashifika/comic-auto-resize/blob/master/README.md)
for the tool as it currently ships.

## Why rewrite

The Go implementation holds every page of an archive in memory at once, so peak usage
grows with page count rather than with page size. It also carries four checked-in static
libraries to link mozjpeg. The rewrite streams pages through a bounded pipeline, so peak
memory is a function of worker count alone, and builds mozjpeg from source.

Behaviour is otherwise intended to match v1.1.2, with a handful of confirmed defects
corrected rather than reproduced.

## Contributing

[`CONTRIBUTING.md`](CONTRIBUTING.md) covers the toolchain, how to build, the verification
sequence to run before opening a pull request, and the branch flow.
[`CLAUDE.md`](CLAUDE.md) covers the architecture and the decisions behind it.

## Licence

Apache-2.0. See [`LICENSE`](LICENSE).
