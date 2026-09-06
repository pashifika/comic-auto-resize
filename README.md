# comic-auto-resize

Shrink comic archives by resizing their pages, keeping line art clear and saving storage space.

## Features

- **Archive inputs:** read zip, rar, 7z, or a directory of pages; write a zip archive.
- **Image inputs:** read JPEG, PNG, BMP, and WebP pages; encode resized pages as JPEG.
- **Resize choices:** fit pages to a common width or reduce each by a percentage.
- **Originals kept:** write a separate archive; remove the input only when you request it.

## Build from source

Install Git, [rustup](https://rustup.rs/), and a C/C++ compiler; x86 builds also need `nasm`.
See [prerequisites](CONTRIBUTING.md#prerequisites) for platform setup and Git authentication.
The commands below use Bash (Git Bash on Windows).

```bash
git clone --branch dev/2.0.x https://github.com/pashifika/comic-auto-resize.git
cd comic-auto-resize
cargo build --locked --release
./target/release/comic-auto-resize --help
```

The executable is `target/release/comic-auto-resize` (`comic-auto-resize.exe` on Windows).
There is no packaged release of this Rust version yet. The first build takes minutes because
it compiles mozjpeg from source.

## Usage

Place a copy of your comic archive in the cloned directory as `volume.zip`, then run:

```bash
./target/release/comic-auto-resize volume.zip
```

This writes `volume_resize.zip` beside the input, targeting a width of 1280 pixels.
It keeps the original. Pages are never enlarged; pages that are too small to resize safely
are kept unchanged. Existing output files are refused, not overwritten.

To reduce each page to 70% of its own width instead:

```bash
./target/release/comic-auto-resize -r 70 -o volume-70.cbz volume.zip
```

This writes a zip archive named `volume-70.cbz`; no extra extension is added.
For a 960-pixel target with baseline JPEG pages for older viewers:

```bash
./target/release/comic-auto-resize --auto-width 960 --progressive=false -o volume-960.zip volume.zip
```

This writes `volume-960.zip`. The height follows each page's aspect ratio.
Use an archive or a directory path in place of `volume.zip` for your own books.

## Options

These are the binary's long options. Run `--help` for accepted values, defaults, and trade-offs.

| Option | Summary |
|---|---|
| `--out <PATH>` | Output filename, or an existing directory for the default name (`-o`). |
| `--delete-org` | Delete the input archive only after successful output; refused for directories. |
| `--auto-width <PIXELS>` | Target a common page width; defaults to 1280. |
| `--ratio <PERCENT>` | Use 1–100% of each page's width (`-r`); cannot combine with `--auto-width`. |
| `--quality <QUALITY>` | Set JPEG quality from 1 to 100 (`-q`); defaults to 90. |
| `--dct <DCT>` | Select the JPEG DCT/IDCT method. |
| `--progressive[=<BOOL>]` | Write progressive JPEGs by default; `--progressive=false` selects baseline. |
| `--optimizer[=<BOOL>]` | Optimise entropy coding by default; disabling takes effect with baseline JPEGs. |
| `--resize-mode <RESIZE_MODE>` | Select interpolation; defaults to `lanczos3`. |
| `--fix-idx` | Replace trailing page numbers with read-order positions within each directory. |
| `--charset <LIST>` | Try encodings for undeclared archive entry names; defaults to `ja,zh`. |
| `--pwd <PASSWORD>` | Decrypt ZipCrypto zip or encrypted rar; AES zip and encrypted 7z are unsupported. |
| `--jobs <COUNT>` | Set parallel page workers; lower the count to reduce memory use. |
| `--completions <SHELL>` | Print a Bash, Zsh, Fish, or PowerShell completion script; use alone. |
| `--help` | Print help (`-h`). |
| `--version` | Print the version (`-V`). |

## Coming from v1.1.2

- `-r 70` now means 70% of each page's width, not “fit to 1280”. Remove it to keep the old width target.
- `-r` has no default percentage. Without it, `--auto-width` controls resizing and defaults to 1280.
- `--small-skip` is absent. Small-page protection is always applied; the old flag disabled all resizing, not just small-page resizing.
- `-o out.cbz` writes `out.cbz`, not `out.cbz.zip`.

## Requirements

Native verification targets Windows x86-64 and Apple Silicon macOS. Building requires Rust 1.93
or newer; `rust-toolchain.toml` selects the tested toolchain through rustup. Build dependencies
and platform setup are in [CONTRIBUTING.md](CONTRIBUTING.md#prerequisites).

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md) covers builds, verification, and pull requests.
[CLAUDE.md](CLAUDE.md) covers architecture and project decisions.

## Licence

[Apache-2.0](LICENSE). [NOTICE.md](NOTICE.md) carries third-party notices.
