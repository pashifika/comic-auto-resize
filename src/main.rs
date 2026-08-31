//! Command-line entry point.
//!
//! The surface contains exactly what is implemented. A flag may exist and be unimplemented,
//! or not exist; it must not exist and silently do the wrong thing — so the flags Go had and
//! this build does not (`--delete-org`, `-o/--out`, `-r/--ratio`, `--small-skip`,
//! `--optimizer`, `--progressive`) are absent rather than accepted and ignored. Absence is the
//! honest form of "not yet".
//!
//! `--fix-idx` was the first flag added since the rewrite began, and `--charset` and `--pwd`
//! join it in the Change that implements them, which is that rule read the other way round.
//!
//! `--charset` is the first flag whose default is not "off", and the asymmetry is the point:
//! `--fix-idx` defaults to off because the default path is *correct* and renaming is a
//! preference, while here the default path is wrong — it decodes a Japanese archive's names as
//! CP437 and turns a page into a subdirectory. A flag defaults to off when what it changes is
//! a choice, and to on when what it changes is a defect.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;

use clap::Parser;
use comic_auto_resize::page::{DctMethod, DecodeSettings, EncodeSettings, Filter};
use comic_auto_resize::pipeline::{self, Settings};
use comic_auto_resize::policy::AUTO_WIDTH;
use comic_auto_resize::sink::{InputKind, default_output};
use comic_auto_resize::source::{Charset, DEFAULT_LABELS, Naming, ReadOptions, Source};
use thiserror::Error;

/// The largest width a JPEG can express, so the largest worth accepting.
const MAX_WIDTH: i64 = 65535;

/// Auto-resize the pages of a comic archive and repack them as zip.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// The comic archive or directory of pages to shrink. A new archive is written beside
    /// it, with `_resize` appended to the name.
    input: PathBuf,

    /// Normalise every page to this width in pixels; the height follows the page's aspect
    /// ratio.
    #[arg(
        long,
        default_value_t = AUTO_WIDTH,
        value_parser = clap::value_parser!(u32).range(1..=MAX_WIDTH),
        value_name = "PIXELS",
    )]
    auto_width: u32,

    /// Encoder quality, 1 to 100.
    #[arg(
        short,
        long,
        default_value_t = 90,
        value_parser = clap::value_parser!(u8).range(1..=100),
    )]
    quality: u8,

    /// JPEG DCT/IDCT method.
    #[arg(long, default_value = DctMethod::default().name(), value_parser = DctMethod::NAMES)]
    dct: String,

    /// Resize interpolation mode.
    #[arg(long, default_value = Filter::default().name(), value_parser = Filter::NAMES)]
    resize_mode: String,

    /// Rewrite each page's name to carry its own position: the trailing digits of the name
    /// are replaced by the page's place in read order, restarting at one inside each
    /// directory, zero-padded to the width the entry total needs. The number the input
    /// recorded is not consulted and a name with no trailing digits is left alone. Off by
    /// default; enable it when a viewer orders pages by name rather than numerically.
    #[arg(long)]
    fix_idx: bool,

    /// Encodings to try for an entry name the archive stores without declaring its encoding,
    /// in order. The list is consulted only where the container declares none — a zip's
    /// UTF-8 flag and its Info-ZIP Unicode Path field both outrank it — and one encoding is
    /// chosen for the whole input, so no two pages of one book are decoded differently. Takes
    /// `ja`, `zh`, `ko` or any WHATWG label an ASCII-compatible encoding answers to, such as
    /// `shift_jis` or `gb18030`; a label naming one that is not — `utf-16le`, `iso-2022-jp` —
    /// is refused, because a name decoded through it would lose its own extension. Pass an
    /// empty value to choose none and leave such names as the format's historical default.
    #[arg(long, default_value = DEFAULT_LABELS, value_parser = charset, value_name = "LIST")]
    charset: Charset,

    /// Password for an encrypted archive. This build decrypts a zip's `ZipCrypto` entries and
    /// rar's encrypted entries; a zip encrypted with AES and a 7z, whose only encryption is
    /// `AES-256`, are refused by name rather than read.
    #[arg(long, value_name = "PASSWORD")]
    pwd: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok((pages, output)) => {
            println!("{pages} page(s) written to {}", output.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            // Errors to stderr, so the success line stays pipeable.
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<(u32, PathBuf), CliError> {
    // Option values were range-checked by the parser, before this point and before the
    // input is opened. The remaining checks are on the input itself.
    let settings = Settings {
        jobs: worker_count(),
        target_width: cli.auto_width,
        filter: cli.resize_mode.parse().map_err(CliError::Filter)?,
        decode: DecodeSettings {
            dct_method: cli.dct.parse().map_err(CliError::Dct)?,
            ..DecodeSettings::default()
        },
        encode: EncodeSettings {
            quality: cli.quality,
            dct_method: cli.dct.parse().map_err(CliError::Dct)?,
            ..EncodeSettings::default()
        },
    };
    // Every option is settled before the input is opened: an unknown `--charset` label was
    // refused by the parser, and the encoding list is resolved rather than a string the reader
    // would have to parse per archive.
    let options = ReadOptions {
        naming: if cli.fix_idx {
            Naming::ByPosition
        } else {
            Naming::Stored
        },
        charset: cli.charset.clone(),
        password: cli.pwd.clone(),
    };

    // The input's *kind* is established before its format, and both before anything is
    // written: a directory is an input in its own right and has no leading bytes to probe,
    // while a file's reader is decided by those bytes and never by its extension — `.cbz`
    // and `.cbr` are conventions that the tools writing them get mixed up.
    //
    // For zip and 7z the header is read here too, so a malformed archive fails before the
    // output is created; for a directory the listing is made here, so an unreadable tree
    // does the same. No `BufReader` for zip: `by_index` seeks to every entry and
    // `BufReader::seek` throws its buffer away, so a wrapper would be discarded once a page.
    // The table's own reads are small and unbuffered — measured at 2 ms for 1000 entries.
    let source = Source::open(&cli.input, &options).map_err(|source| CliError::Archive {
        path: cli.input.clone(),
        source,
    })?;
    let kind = match source {
        Source::Directory(_) => InputKind::Directory,
        _ => InputKind::File,
    };
    let output = default_output(&cli.input, kind)?;

    // A `SourceError` raised during iteration would otherwise reach the user through two
    // transparent wrappers with no path at all, while the same error raised inside
    // `Source::open` arrives as `{path}: {source}`. rar is where that shows: it walks headers
    // as it goes, so a damaged entry surfaces here rather than at open.
    let report = pipeline::run(source, &output, &settings).map_err(|error| match error {
        pipeline::RunError::Source(source) => CliError::Archive {
            path: cli.input.clone(),
            source,
        },
        other => CliError::Run(other),
    })?;
    Ok((report.pages, output))
}

/// How many pages are processed at once.
///
/// Mirrors the Go implementation: all but one core once there are five, and four below that.
/// Not a flag, because `--jobs` is out of this change's scope; the pipeline's peak memory is
/// a function of this number, so it becomes an option only alongside a bound on it.
fn worker_count() -> NonZeroUsize {
    let cpus = thread::available_parallelism().map_or(4, NonZeroUsize::get);
    let jobs = if cpus >= 5 { cpus - 1 } else { 4 };
    NonZeroUsize::new(jobs).unwrap_or(NonZeroUsize::MIN)
}

/// Resolves `--charset`'s label list, so an unknown label is refused before the input opens.
///
/// Named rather than inlined because `clap`'s derive wants a function path, and the resolution
/// belongs to the reader's rule rather than to the parser: `main` supplies only the default.
fn charset(labels: &str) -> Result<Charset, comic_auto_resize::source::BadLabel> {
    Charset::resolve(labels)
}

#[derive(Debug, Error)]
enum CliError {
    /// `NotAFile` and `Input` are gone with the `metadata` call that raised them: the input's
    /// kind is the reader's question now, because a directory is an input and "not a file"
    /// stopped being the right refusal. Both reach the user through `Archive`, which prefixes
    /// the path exactly as they did.
    ///
    /// `NotZip` went the same way one Change earlier: which formats the build reads is the
    /// reader's knowledge, so the refusal is `SourceError::NotAnArchive` and names them.
    /// Named with the path, because a header is read when the input is opened and a
    /// malformed one is a property of the file rather than of an entry.
    #[error("{}: {source}", path.display())]
    Archive {
        path: PathBuf,
        source: comic_auto_resize::source::SourceError,
    },
    #[error(transparent)]
    Filter(comic_auto_resize::page::UnknownFilter),
    #[error(transparent)]
    Dct(comic_auto_resize::page::UnknownDctMethod),
    #[error(transparent)]
    Run(#[from] pipeline::RunError),
}
