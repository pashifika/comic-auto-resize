//! Command-line entry point.
//!
//! The surface contains exactly what is implemented. A flag may exist and be unimplemented,
//! or not exist; it must not exist and silently do the wrong thing — so the flags Go had and
//! this build does not (`--charset`, `--pwd`, `--delete-org`, `-o/--out`, `-r/--ratio`,
//! `--small-skip`, `--optimizer`, `--progressive`) are absent rather than accepted and
//! ignored. Absence is the honest form of "not yet".

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;

use clap::Parser;
use comic_auto_resize::page::{DctMethod, DecodeSettings, EncodeSettings, Filter};
use comic_auto_resize::pipeline::{self, Settings};
use comic_auto_resize::policy::AUTO_WIDTH;
use comic_auto_resize::sink::default_output;
use comic_auto_resize::source::Source;
use thiserror::Error;

/// The largest width a JPEG can express, so the largest worth accepting.
const MAX_WIDTH: i64 = 65535;

/// Auto-resize the pages of a comic archive and repack them as zip.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// The comic archive to shrink. A new archive is written beside it, with `_resize`
    /// appended to the name.
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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(pages) => {
            println!(
                "{} page(s) written to {}",
                pages,
                default_output(&cli.input).display()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            // Errors to stderr, so the success line stays pipeable.
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<u32, CliError> {
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

    // The input is established as a readable file before anything is opened, so a missing or
    // wrong input names itself before the output file is created. Which reader runs is
    // decided by the file's leading bytes, never by its extension: `.cbz` and `.cbr` are
    // conventions that the tools writing them get mixed up.
    //
    // For zip the entry table is read here too, so a malformed archive fails before the
    // output is created. No `BufReader`: `by_index` seeks to every entry and `BufReader::seek`
    // throws its buffer away, so a wrapper would be discarded once a page. The table's own
    // reads are small and unbuffered — measured at 2 ms for 1000 entries.
    let metadata = std::fs::metadata(&cli.input).map_err(|source| CliError::Input {
        path: cli.input.clone(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(CliError::NotAFile {
            path: cli.input.clone(),
        });
    }
    let source = Source::open(&cli.input).map_err(|source| CliError::Archive {
        path: cli.input.clone(),
        source,
    })?;
    let output = default_output(&cli.input);
    let report = pipeline::run(source, &output, &settings)?;
    Ok(report.pages)
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

#[derive(Debug, Error)]
enum CliError {
    #[error("{}: {source}", path.display())]
    Input {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{}: not a file", path.display())]
    NotAFile { path: PathBuf },
    /// `NotZip` is gone with `open_zip`: which formats the build reads is the reader's
    /// knowledge, so the refusal is `SourceError::NotAnArchive` and names them.
    /// Named with the path, because the entry table is read when the input is opened and a
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
