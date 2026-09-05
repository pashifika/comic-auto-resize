//! Command-line entry point.
//!
//! The surface contains exactly what is implemented. A flag may exist and be unimplemented,
//! or not exist; it must not exist and silently do the wrong thing — so the three flags Go had
//! and this build does not are absent rather than accepted and ignored. `--small-skip` will
//! stay absent: Go's implementation of it is `if !skipSmallSize && doResize`, so passing it
//! disables resizing for every page rather than skipping the small-ratio ones its name
//! promises, and the behaviour the name describes is what `policy.rs`'s `MIN_EDGE` floor does
//! unconditionally. `--show-time` and `--debug` are Go's `Developer Options` group and measure
//! or instrument that build rather than describe an archive. Absence is the honest form of
//! "not yet", and for these three it is the honest form of "no".
//!
//! `--fix-idx` was the first flag added since the rewrite began; `--charset` and `--pwd`
//! joined it, then `-o/--out` and `--delete-org`, then `-r/--ratio` and `--jobs`, and
//! `--progressive` and `--optimizer` join now, each in the Change that implements it — which
//! is that rule read the other way round.
//!
//! `--jobs` is the one flag here with no reference-tool equivalent: the Go implementation
//! derives its worker count from the host and offers no way to say otherwise. `-r/--ratio`
//! is the one whose meaning deliberately diverges, and its help says so, because that is
//! where the user who needs to know is looking.
//!
//! `--charset` is the first flag whose default is not "off", and the asymmetry is the point:
//! `--fix-idx` defaults to off because the default path is *correct* and renaming is a
//! preference, while here the default path is wrong — it decodes a Japanese archive's names as
//! CP437 and turns a page into a subdirectory. A flag defaults to off when what it changes is
//! a choice, and to on when what it changes is a defect. `--delete-org` removes the user's
//! input, which is the most destructive choice on the surface, so it is off.
//!
//! `--progressive` and `--optimizer` are the two whose bare form is a no-op, and deliberately:
//! they take an optional value and default to on, so the reference tool's spelling parses and
//! asserts the state its help promised, while `=false` is the switch. The value is attached
//! with `=` because an optional value taken from the next argument would swallow the
//! positional input path.
//!
//! Their polarity is a third case beside the two above, and it is why the rule does not settle
//! it: neither is a defect correction — baseline output is a compatibility and memory trade,
//! not a repair — and neither is an open choice this build gets to make freshly. They default
//! to on because that is what the reference tool's help documented and what this build has
//! written since `native-deps`, so the default is inherited rather than chosen.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::LazyLock;
use std::thread;

use clap::{ArgAction, CommandFactory, Parser, builder::TypedValueParser};
use comic_auto_resize::page::{DctMethod, DecodeSettings, EncodeSettings, Filter};
use comic_auto_resize::pipeline::{self, Report, Settings};
use comic_auto_resize::policy::{AUTO_WIDTH, Target};
use comic_auto_resize::sink::{InputKind, durable_directory_entry, resolve_output};
use comic_auto_resize::source::{Charset, DEFAULT_LABELS, Naming, ReadOptions, Source};
use thiserror::Error;

mod completion;

/// The largest width a JPEG can express, so the largest worth accepting.
const MAX_WIDTH: i64 = 65535;

/// Auto-resize the pages of a comic archive and repack them as zip.
///
/// Four bools, which is one past what `clippy::pedantic` allows a struct. The lint's remedy —
/// a state machine, or two-variant enums — does not apply to this one: the fields are not
/// state, they are the command-line surface itself, one per flag, and `clap`'s derive reads
/// the type to decide how the flag parses. An enum here would produce a different surface and
/// then have to be converted back. `expect` rather than `allow`, so a Change that drops a
/// switch is told the exemption is no longer needed.
#[expect(
    clippy::struct_excessive_bools,
    reason = "one field per command-line flag; the type decides how clap parses it"
)]
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// The comic archive or directory of pages to shrink. The output is written beside it as
    /// `<stem>_resize.zip` unless `-o` names somewhere else.
    //
    // `Option` only so `--completions` can be given without one; every other invocation is
    // refused by the parser before `main` sees it.
    #[arg(required_unless_present = "completions")]
    input: Option<PathBuf>,

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

    /// Reduce every page to this percentage of its own width, 1 to 100; the height follows
    /// the page's aspect ratio. Cannot be given with `--auto-width`, which names the same
    /// quantity absolutely. The reference tool's `-r 70` does *not* mean seventy per cent —
    /// it normalises to 1280 — and normalising to 1280 is what this tool does when told
    /// nothing, so an invocation that carried `-r 70` wants no flag at all.
    #[arg(
        short,
        long,
        conflicts_with = "auto_width",
        value_parser = clap::value_parser!(u8).range(1..=100),
        value_name = "PERCENT",
    )]
    ratio: Option<u8>,

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

    /// Write progressive JPEG pages; pass `--progressive=false` for baseline. On by default,
    /// matching the reference tool's documented default — its binary applied the opposite, so
    /// the same command line produces different bytes here. Baseline costs size rather than
    /// saving it: measured across this project's sample archives it is 1.8 to 5.4 per cent
    /// larger. What it buys is compatibility and read-back memory — a progressive page costs
    /// about 4.3x a baseline one to decode — so `=false` suits an archive this tool, or an
    /// older viewer, will read again.
    #[arg(
        long,
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value_t = true,
        value_name = "BOOL",
    )]
    progressive: bool,

    /// Optimise the entropy-coding tables: costs an encoding pass, saves bytes. On by
    /// default. While a progressive file is written libjpeg forces this on regardless, so
    /// `--optimizer=false` takes effect together with `--progressive=false`.
    #[arg(
        long,
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value_t = true,
        value_name = "BOOL",
    )]
    optimizer: bool,

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

    /// How many pages are decoded, resized and encoded at once, at most twice this host's
    /// available parallelism and never fewer than four. Peak memory scales with this: roughly
    /// the worker count times the largest page's decoded working set, which measured 2.59 GB
    /// for nine workers on nine 4608x7281 webp pages. Lower it on a machine that cannot spare
    /// that; the full term is in `src/pipeline.rs`. The reference tool derives this number and
    /// offers no way to set it.
    #[arg(long, default_value_t = worker_count(), value_parser = jobs, value_name = "COUNT")]
    jobs: NonZeroUsize,

    /// Write this shell's completion script to standard output and exit. Takes `bash`,
    /// `zsh`, `fish` or `powershell`, and nothing else on the command line: no input is
    /// opened and no filesystem state is read, because a script is generated while a shell
    /// starts. The script comes from the same command graph `--help` does, so a flag cannot
    /// be completed without existing.
    //
    // A flag rather than a subcommand, and the difference was measured rather than chosen
    // on taste: a subcommand makes `clap_complete`'s fish generator guard every root option
    // with `__fish_<name>_needs_command`, whose `argparse` run fails on the half-typed
    // `--dct` it is being asked about — so fish silently offered filenames where `float`,
    // `ifast` and `islow` belong. No subcommand, no guard, and the values come back.
    //
    // `exclusive` because a completion request accepts nothing else: `-q 80 --completions
    // bash` names a quality no script has any use for, and the surface's founding rule is
    // that a flag is not accepted and then ignored.
    #[arg(long, value_name = "SHELL", exclusive = true)]
    completions: Option<completion::Shell>,
}

/// The one command graph, reached by the parser and by the completion generator alike.
///
/// `clap`'s derive is the single definition of it: [`Parser::parse`] parses with exactly what
/// [`CommandFactory::command`] returns here. That is the whole mechanism behind "a flag
/// cannot appear in completion without existing" — there is no second list to keep in step.
fn command() -> clap::Command {
    Cli::command()
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(shell) = cli.completions {
        return match completion::write(shell, &mut io::stdout().lock()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let Some(input) = cli.input.as_deref() else {
        // Unreachable through the parser: `required_unless_present` makes the positional
        // required for every invocation that is not the one handled above. Routed through
        // `clap`'s own error rather than a panic, so a surface change that made it reachable
        // would tell the user what is missing instead of aborting.
        command()
            .error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "the following required arguments were not provided:\n  <INPUT>",
            )
            .exit()
    };
    match run(&cli, input) {
        Ok((report, output)) => {
            // One line for the run, and each extra clause only when the extra thing
            // happened: a run that composited nothing, refused nothing and removed nothing
            // prints exactly what it printed before any of those rules existed. A line per
            // page would bury the page count on a real archive.
            let mut notes = Vec::new();
            if report.composited > 0 {
                notes.push(format!(
                    "{} page(s) composited onto white",
                    report.composited
                ));
            }
            // What happened, not a failure: the page is in the output at full size because
            // the reduction asked for would have left an edge under the floor.
            if report.below_floor > 0 {
                notes.push(format!(
                    "{} page(s) too small to shrink, kept at full size",
                    report.below_floor
                ));
            }
            let notes = if notes.is_empty() {
                String::new()
            } else {
                // Joined with a semicolon rather than a comma, because a clause may carry a
                // comma of its own and two facts must not read as three fragments.
                format!(" ({})", notes.join("; "))
            };
            // `Ok` and `--delete-org` together mean the input is gone: a removal that failed
            // is an error, so there is no third state to report.
            let removed = if cli.delete_org {
                format!("; {} removed", input.display())
            } else {
                String::new()
            };
            println!(
                "{} page(s) written to {}{notes}{removed}",
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

/// The resize run. `input` is the positional the parser required, passed separately because
/// the completion entry point is the one arm of the surface that has none.
fn run(cli: &Cli, input: &Path) -> Result<(Report, PathBuf), CliError> {
    // Option values were range-checked by the parser, before this point and before the
    // input is opened. The remaining checks are on the input itself.
    let settings = Settings {
        jobs: cli.jobs,
        // The parser refused the two together, so this is a choice between them rather than
        // a precedence nobody asked for.
        target: match cli.ratio {
            Some(percent) => Target::Ratio(percent),
            None => Target::Width(cli.auto_width),
        },
        filter: cli.resize_mode.parse().map_err(CliError::Filter)?,
        decode: DecodeSettings {
            dct_method: cli.dct.parse().map_err(CliError::Dct)?,
            ..DecodeSettings::default()
        },
        // Every field named, and no `..default()` spread: each of the four settings the
        // encoder carries now has a flag, so a spread here would only hide which of them the
        // command line reaches.
        encode: EncodeSettings {
            quality: cli.quality,
            optimize_coding: cli.optimizer,
            progressive: cli.progressive,
            dct_method: cli.dct.parse().map_err(CliError::Dct)?,
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
        match fs::symlink_metadata(input) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(CliError::DeleteSymbolicLink {
                    path: input.to_path_buf(),
                });
            }
            Ok(_) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(CliError::UnidentifiedInput {
                    path: input.to_path_buf(),
                    source,
                });
            }
        }
    }
    let source = Source::open(input, &options).map_err(|source| CliError::Archive {
        path: input.to_path_buf(),
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
            path: input.to_path_buf(),
        });
    }
    let output = resolve_output(input, kind, cli.out.as_deref())?;

    // A `SourceError` raised during iteration would otherwise reach the user through two
    // transparent wrappers with no path at all, while the same error raised inside
    // `Source::open` arrives as `{path}: {source}`. rar is where that shows: it walks headers
    // as it goes, so a damaged entry surfaces here rather than at open.
    let report = pipeline::run(source, &output, &settings).map_err(|error| match error {
        pipeline::RunError::Source(source) => CliError::Archive {
            path: input.to_path_buf(),
            source,
        },
        other => CliError::Run(other),
    })?;

    // `pipeline::run` took the source by value and dropped it before returning, so this
    // process holds no handle to the input — which is what makes the removal safe on Windows.
    // Nothing is removed unless `Ok` came back, which means the archive was written and
    // flushed in the file it will be delivered as.
    if cli.delete_org {
        // The bytes are durable — `Sink::finish` flushed them through the handle that wrote
        // them — but on unix the *entry naming them* is not, because `fsync` on a file says
        // nothing about its parent directory. The name was created before the whole
        // conversion rather than a moment ago, which makes the window a different shape from
        // the rename this replaced, but it is still an unflushed entry and the input is about
        // to become unrecoverable. So the parent is flushed here, on the one path that has
        // something to lose.
        durable_directory_entry(&output).map_err(|source| CliError::OutputNotDurable {
            output: output.clone(),
            path: input.to_path_buf(),
            source,
        })?;
        fs::remove_file(input).map_err(|source| CliError::InputNotRemoved {
            output: output.clone(),
            path: input.to_path_buf(),
            source,
        })?;
    }
    Ok((report, output))
}

/// What the reference tool assumes when it cannot read the host: the count it falls back to
/// below five cores.
const DEFAULT_CORES: NonZeroUsize = NonZeroUsize::new(4).expect("four is not zero");

/// What the host says it can run in parallel, or [`DEFAULT_CORES`] when it will not say.
///
/// One answer to one question, and taken **once**: both the default and the ceiling derive
/// from this, `available_parallelism` is documented to track limits that can change while the
/// process runs, and `clap` re-parses the default it published through the same value parser.
/// Two readings could therefore straddle a cgroup or affinity change and leave the parser
/// refusing its own default — on the bare invocation, which is the path that must never fail.
/// An earlier form read it twice with two different fallbacks, which agreed only by arithmetic
/// accident.
static HOST_CORES: LazyLock<NonZeroUsize> =
    LazyLock::new(|| thread::available_parallelism().unwrap_or(DEFAULT_CORES));

/// How many pages are processed at once by default.
///
/// Mirrors the Go implementation: all but one core once there are five, and four below that.
/// `--jobs` overrides it and defaults to it, so the flag existing does not move the default:
/// the pipeline's peak memory is a function of this number, and the flag's help is where the
/// cost of raising it is stated.
fn worker_count() -> NonZeroUsize {
    worker_count_for(*HOST_CORES)
}

/// The largest worker count worth accepting: twice the host's available parallelism, and
/// never below the default.
///
/// Bounded rather than open. Each page is decoded, resized and encoded on its worker, so past
/// the host's own parallelism another worker buys no throughput and costs its whole
/// per-worker term in memory; twice it is headroom for a host whose reported parallelism
/// understates what it will run. Past that the machine answers instead of the tool — a
/// thread-spawn failure or an allocation failure rather than a refusal naming the value —
/// which is the wrong shape of answer for a value the parser can see.
///
/// The reference tool has no ceiling because it has no flag: `errgroup.WithContext(ctx, cpus)`
/// bounds its own concurrency at the same derived count and there is no way to ask for more.
/// So every count this accepts above the default is already more than that build could run.
fn max_jobs() -> NonZeroUsize {
    max_jobs_for(*HOST_CORES)
}

/// The default for a host of `cores`, split out so the invariant below can be tested.
fn worker_count_for(cores: NonZeroUsize) -> NonZeroUsize {
    let cores = cores.get();
    let jobs = if cores >= 5 { cores - 1 } else { 4 };
    NonZeroUsize::new(jobs).unwrap_or(NonZeroUsize::MIN)
}

/// The ceiling for a host of `cores`.
///
/// **Never below [`worker_count_for`] of the same host**, and that is load-bearing rather than
/// tidy: `default_value_t` is re-parsed through the same `value_parser`, so a default above
/// the ceiling would make the parser refuse its own default and every bare invocation would
/// exit non-zero. A host of two cores is where the two meet — four either way — because the
/// reference tool's floor of four workers applies below five cores whatever the host reports.
fn max_jobs_for(cores: NonZeroUsize) -> NonZeroUsize {
    let ceiling = cores
        .get()
        .saturating_mul(2)
        .max(worker_count_for(cores).get());
    NonZeroUsize::new(ceiling).unwrap_or(NonZeroUsize::MIN)
}

/// Refuses a worker count the host cannot use, before the input is opened.
///
/// Zero is refused by the type — a run with no workers is not a run — and the ceiling by
/// [`max_jobs`]. Named rather than inlined for the reason `charset` is: `clap`'s derive wants
/// a function path.
fn jobs(value: &str) -> Result<NonZeroUsize, String> {
    let jobs: NonZeroUsize = value
        .parse()
        .map_err(|_| format!("`{value}` is not a worker count"))?;
    let ceiling = max_jobs();
    if jobs > ceiling {
        return Err(format!(
            "{jobs} is more than this host can use; the most is {ceiling}"
        ));
    }
    Ok(jobs)
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

#[cfg(test)]
mod tests {
    use super::{DEFAULT_CORES, max_jobs_for, worker_count_for};
    use std::num::NonZeroUsize;

    /// The default is never above the ceiling, on any host.
    ///
    /// `default_value_t` is re-parsed through the same `value_parser` the flag uses, so a
    /// host where the default exceeded the ceiling would make `clap` refuse its own default
    /// and every bare invocation would exit non-zero. The ceiling's `.max()` is what
    /// guarantees it, and one and two cores are the only rows where that clause is what
    /// decides — twice one core is 2 and twice two is 4 against the reference tool's floor of
    /// four workers.
    #[test]
    fn the_default_worker_count_is_inside_the_ceiling_on_every_host() {
        for cores in [1usize, 2, 3, 4, 5, 6, 9, 10, 64, 256, usize::MAX] {
            let cores = NonZeroUsize::new(cores).expect("non-zero");
            let default = worker_count_for(cores);
            let ceiling = max_jobs_for(cores);
            assert!(
                default <= ceiling,
                "{cores} core(s): default {default} is above the ceiling {ceiling}"
            );
        }
    }

    /// The derivation the reference tool uses, at the boundary it turns on.
    #[test]
    fn the_default_mirrors_the_reference_tools_derivation() {
        for (cores, expected) in [(1, 4), (2, 4), (4, 4), (5, 4), (6, 5), (10, 9)] {
            let cores = NonZeroUsize::new(cores).expect("non-zero");
            assert_eq!(worker_count_for(cores).get(), expected, "{cores} core(s)");
        }
        // A host that cannot be read is treated as the count the reference tool falls back
        // to, so the default is the same four workers either way.
        assert_eq!(worker_count_for(DEFAULT_CORES).get(), 4);
    }
}
