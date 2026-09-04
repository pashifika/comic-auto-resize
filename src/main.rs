//! Command-line entry point.
//!
//! The surface contains exactly what is implemented. A flag may exist and be unimplemented,
//! or not exist; it must not exist and silently do the wrong thing — so the flags Go had and
//! this build does not (`-r/--ratio`, `--small-skip`, `--optimizer`, `--progressive`) are
//! absent rather than accepted and ignored. Absence is the honest form of "not yet".
//!
//! `--fix-idx` was the first flag added since the rewrite began; `--charset` and `--pwd`
//! joined it, and `-o/--out` and `--delete-org` join now, each in the Change that implements
//! it — which is that rule read the other way round.
//!
//! `--charset` is the first flag whose default is not "off", and the asymmetry is the point:
//! `--fix-idx` defaults to off because the default path is *correct* and renaming is a
//! preference, while here the default path is wrong — it decodes a Japanese archive's names as
//! CP437 and turns a page into a subdirectory. A flag defaults to off when what it changes is
//! a choice, and to on when what it changes is a defect. `--delete-org` removes the user's
//! input, which is the most destructive choice on the surface, so it is off.

use std::ffi::OsString;
use std::fs;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;

use clap::{Parser, builder::TypedValueParser};
use comic_auto_resize::page::{DctMethod, DecodeSettings, EncodeSettings, Filter};
use comic_auto_resize::pipeline::{self, Report, Settings};
use comic_auto_resize::policy::AUTO_WIDTH;
use comic_auto_resize::sink::{InputKind, durable, resolve_output};
use comic_auto_resize::source::{Charset, DEFAULT_LABELS, Naming, ReadOptions, Source};
use thiserror::Error;

/// The largest width a JPEG can express, so the largest worth accepting.
const MAX_WIDTH: i64 = 65535;

/// Auto-resize the pages of a comic archive and repack them as zip.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// The comic archive or directory of pages to shrink. The output is written beside it as
    /// `<stem>_resize.zip` unless `-o` names somewhere else.
    input: PathBuf,

    /// Where to write the output archive. A value naming a location — it ends with a path
    /// separator, or it is an existing directory — has the default name `<stem>_resize.zip`
    /// joined to it; any other value is used as the filename verbatim, with no extension
    /// appended, so `-o out.cbz` writes a zip archive called `out.cbz`. The directory must
    /// already exist. The reference tool appends `.zip` to this value; this one does not.
    #[arg(
        short,
        long,
        value_name = "PATH",
        value_parser = clap::builder::OsStringValueParser::new().try_map(output),
    )]
    out: Option<PathBuf>,

    /// Remove the input archive once the output archive is in place. Nothing is removed if
    /// the run failed. Refused when the input is a directory: this removes the archive it
    /// read, not a tree.
    #[arg(long)]
    delete_org: bool,

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
        Ok((report, output)) => {
            // One line for the run, and each extra clause only when the extra thing
            // happened: a run that composited nothing and removed nothing prints exactly
            // what it printed before either rule existed. A line per page would bury the
            // page count on a real archive.
            let composited = if report.composited > 0 {
                format!(" ({} page(s) composited onto white)", report.composited)
            } else {
                String::new()
            };
            // `Ok` and `--delete-org` together mean the input is gone: a removal that failed
            // is an error, so there is no third state to report.
            let removed = if cli.delete_org {
                format!("; {} removed", cli.input.display())
            } else {
                String::new()
            };
            println!(
                "{} page(s) written to {}{composited}{removed}",
                report.pages,
                output.display()
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

fn run(cli: &Cli) -> Result<(Report, PathBuf), CliError> {
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
    // The symbolic-link refusal comes *before* the open, and needs to. `Source::open` follows
    // the link and reads the archive it points at, while `fs::remove_file` would unlink the
    // link and leave that archive in place — the flag reporting that it removed the input
    // archive while the archive is still there. Nothing about the source is needed to detect
    // that, and opening first would not be free: the 7z reader spawns a decoder that reads its
    // first page before the rendezvous send, so a refusal after the open is not a refusal
    // before a page is read. `symlink_metadata` is what asks, because every other call on this
    // path follows links by design.
    //
    // A query that fails for any reason other than absence is refused too: this is the one
    // flag that destroys a file, and "cannot tell what this path is" is not a licence to
    // delete it. Absence falls through so that `Source::open` below reports the missing input,
    // which is the error the user needs.
    if cli.delete_org {
        match fs::symlink_metadata(&cli.input) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(CliError::DeleteSymbolicLink {
                    path: cli.input.clone(),
                });
            }
            Ok(_) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(CliError::UnidentifiedInput {
                    path: cli.input.clone(),
                    source,
                });
            }
        }
    }
    let source = Source::open(&cli.input, &options).map_err(|source| CliError::Archive {
        path: cli.input.clone(),
        source,
    })?;
    let kind = match source {
        Source::Directory(_) => InputKind::Directory,
        _ => InputKind::File,
    };
    // Refused before the output is resolved and before the pipeline starts: the pipeline reads
    // the page files a directory holds and passes over everything else, so removing the input
    // would take files it never looked at. Go attempts the removal, fails, logs, and exits
    // zero — a flag accepted and then ignored. The kind is the reader's answer, so unlike the
    // link refusal above this one cannot precede the open; a directory input is never 7z, so
    // the eager decoder is not in play here.
    if cli.delete_org && kind == InputKind::Directory {
        return Err(CliError::DeleteDirectory {
            path: cli.input.clone(),
        });
    }
    let output = resolve_output(&cli.input, kind, cli.out.as_deref())?;

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

    // `pipeline::run` took the source by value and dropped it before returning, so this
    // process holds no handle to the input — which is what makes the removal safe on
    // Windows. Nothing is removed unless the output archive reached its final path, which is
    // what `Ok` here means.
    if cli.delete_org {
        // The output is about to become the only copy, so it is made durable first. `rename`
        // ordered the namespace change but nothing flushed the bytes, and a power loss in
        // between would leave the input gone and the output absent or truncated. Only on this
        // path: a run that keeps its input has nothing to lose to that window, and charging
        // every run a flush to close it would be the wrong trade.
        durable(&output).map_err(|source| CliError::OutputNotDurable {
            output: output.clone(),
            path: cli.input.clone(),
            source,
        })?;
        fs::remove_file(&cli.input).map_err(|source| CliError::InputNotRemoved {
            output: output.clone(),
            path: cli.input.clone(),
            source,
        })?;
    }
    Ok((report, output))
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

/// Refuses an empty `-o`, so no value reaching resolution names a path that cannot be
/// printed.
///
/// An empty value is neither arm of the resolution: it names no directory to join a default
/// name to and no file to use verbatim, and every later refusal would have to describe a path
/// with nothing to display. `OsString` rather than `String`, because an output path is not
/// required to be UTF-8 on either release target.
fn output(value: OsString) -> Result<PathBuf, &'static str> {
    if value.is_empty() {
        return Err("an empty value names no path to write");
    }
    Ok(PathBuf::from(value))
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
    /// `--delete-org` with a directory input. The pipeline reads the page files a directory
    /// holds and passes over everything else, so removing the input would take files it never
    /// read; widening it to a recursive delete would promise more than "delete the original".
    /// The reference tool attempts the removal, fails, logs, and exits zero.
    ///
    /// `CliError`'s rather than `RunError`'s because deleting the input is not something the
    /// pipeline does: `main` does it after `pipeline::run` has returned and dropped the
    /// source.
    #[error("{}: is a directory; --delete-org removes the input archive, not a tree", path.display())]
    DeleteDirectory { path: PathBuf },
    /// `--delete-org` where the input's own entry could not be queried for any reason other
    /// than absence. This is the one flag that destroys a file, and a path whose kind cannot
    /// be established is not one to delete: the query that failed is the one distinguishing a
    /// symbolic link from the archive it points at.
    #[error("{}: cannot be identified, so --delete-org will not remove it: {source}", path.display())]
    UnidentifiedInput {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The output is in place and the input is still there. Both facts are named because the
    /// obvious retry would otherwise meet the existing-output refusal and report a second,
    /// unrelated failure; exiting zero would repeat the accepted-and-ignored shape one level
    /// down.
    #[error("{}: written, but the input {} could not be removed: {source}", output.display(), path.display())]
    InputNotRemoved {
        output: PathBuf,
        path: PathBuf,
        source: std::io::Error,
    },
    /// `--delete-org` with a symbolic link as the input. `Source::open` follows the link and
    /// reads the archive it points at, while `fs::remove_file` unlinks the link itself, so the
    /// flag would report that it removed the input archive and leave that archive in place.
    /// Refused rather than silently doing the other thing, and rather than removing the
    /// target, which is a file the user did not name.
    #[error(
        "{}: is a symbolic link; --delete-org would remove the link and leave the archive it points at",
        path.display()
    )]
    DeleteSymbolicLink { path: PathBuf },
    /// The output could not be flushed to disk, so nothing was removed. Fails closed: the
    /// alternative is unlinking the only other copy of the book while the replacement may not
    /// survive a power loss.
    #[error(
        "{}: could not be flushed to disk, so the input {} was not removed: {source}",
        output.display(),
        path.display()
    )]
    OutputNotDurable {
        output: PathBuf,
        path: PathBuf,
        source: std::io::Error,
    },
}
