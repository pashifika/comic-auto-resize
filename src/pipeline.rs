//! The streaming pass: one archive in, one archive out, peak memory a function of the
//! worker count rather than the page count.
//!
//! ```text
//! credits ─────────────────────────────────────────────────────────┐
//!    │ capacity W                                                  │ one per entry written
//!    ▼                                                             │
//! reader (1 thread*) ─► work ─► workers (J) ─► done ─► writer (1 thread)
//!    entry bytes    capacity J   decode      capacity J   BTreeMap ─► ZipWriter
//!                               resize
//!                               encode
//! ```
//!
//! # Why peak memory is `O(J)`
//!
//! The bound is the load-bearing claim, so it is argued rather than asserted.
//!
//! Bounding the two data channels is not enough on its own, and the change's design said it
//! was. Its argument was that at most `2J` completed pages can exist unwritten, because a
//! worker cannot start a page until it has handed off the last one and the output channel
//! holds at most `J`. That misses the `BTreeMap`. The writer has to drain `done` to make any
//! progress, so while entry `k` is still being worked, every later entry that completes is
//! moved *out* of the channel and into the map. Workers are then never blocked, the reader
//! keeps going, and the map grows with the page count — which is exactly the Go behaviour
//! this pass exists to replace, moved one layer down.
//!
//! The `credits` channel is what actually bounds it. It starts holding `W` tokens; the
//! reader takes one before reading an entry and the writer returns one after writing an
//! entry. So at most `W` entries exist anywhere in the system at once — unread but
//! permitted, in `work`, inside a worker, or waiting in the map — and the map alone can hold
//! no more than `W`.
//!
//! It cannot deadlock. The entry the writer is waiting for took its credit before every
//! entry behind it, so it is already in flight and no later entry can take its place; when
//! it is written a credit is returned. `credits` never blocks the writer either: at most `W`
//! tokens exist, and the entry just written holds one of them, so there is always room.
//!
//! ## `W + 1` for a source whose decoder pushes
//!
//! `sevenz-rust2` offers entries rather than being asked for them, so `SevenZSource` decodes
//! an entry and *then* offers it across a rendezvous channel. The credit is taken by
//! `read_entries` when that offer is accepted, so while the decoder is blocked offering, it
//! holds one entry the credit system has not counted. The window for such a source is
//! `W + 1`.
//!
//! One entry, a constant, for one source. It does not grow with the page count, which is
//! what the bound actually claims — but the arithmetic above is precise and would otherwise
//! be wrong. Flipping [`Entries`](crate::source::Entries) to a push shape would not remove
//! the extra entry: in a push shape every source reads before it offers, so all four would
//! pay it instead of one.
//!
//! # What the run's peak actually is, with every factor named
//!
//! The argument above is about the *count* of pages in flight, and it is right about it:
//! measured on 100 pages against 1000 of the same 1520x2150 page, the peak grows by 1.039 for
//! jpg, 1.073 for png, 1.060 for bmp and 1.055 for webp on the build this documents — every one
//! inside the 1.5 the claim allows, and the window needs no change. The same four ratios on the
//! baseline this branched from were 1.043, 1.052, 1.047 and 1.066, so charging the decoder's
//! scratch left the count argument exactly where it was. What that argument did not say is what
//! *one* page in flight costs, and that is decided by which decoder read the archive rather
//! than by the archive.
//!
//! ```text
//! peak ≈ J × worker + W × MAX_ENTRY_BYTES + base
//!
//!   worker   = retained + max(decode, page + destination)
//!   retained = 2 × max over the pages the worker has resized of
//!                  src_width × dst_height × channels
//!   decode   = declared × scratch_factor(format, colour) + page   (page: only where both live)
//! ```
//!
//! `J` is the worker count and `W` the credit window, both in [`Capacities`]. `declared` is
//! `ImageDecoder::total_bytes()`, `scratch_factor` is stated per arm in `page::decode`'s raster
//! module, `page` is the decoded page and `destination` the resampler's output buffer, and
//! `MAX_ENTRY_BYTES` bounds the entry each credit holds.
//!
//! `retained` sits under both stages rather than on one side of the maximum, and that is what
//! this shape corrects. The buffer is `fast_image_resize`'s two-pass scratch
//! (`resizer.rs:418-422`), and it lives in the per-worker `Resizer` rather than in the call, so
//! it survives the page that grew it: a worker holds the previous page's scratch while it
//! decodes the next one. The two stages a worker runs are disjoint in time and this buffer is in
//! neither of them, so it is resident whichever stage the peak falls in, and a maximum that puts
//! it on the resize side loses it whenever the decode side wins. The `2 ×` is the growth bound
//! and not a safety margin: the buffer is grown with `Vec::resize`, whose amortized rule takes
//! `max(2 × old_capacity, required)`, so two pages differing by a single row leave the worker
//! holding twice the larger temporary rather than the temporary itself, and by induction that
//! holds over any number of growths rather than only the first. The allocation is the
//! dependency's and predates this bound; what changed is that the bound now covers it, in the
//! term that is always resident.
//!
//! ## The shape against a run
//!
//! Every earlier statement of this bound was derived and left there. This one was checked: nine
//! pages of 4608x7281 — 33,550,848 pixels, the accepted side of the webp `Rgb8` ceiling, one row
//! under the refusal — through nine workers at the default 1280 target. That makes `dst_height`
//! 2023, so `retained` is `2 × 4608 × 2023 × 3` = 55.93 MB, the page is 100.65 MB and the
//! destination 7.77 MB. The decode side below is the arm's *measured* bytes a source pixel and
//! not its charge, because a charge that deliberately sits above its arm predicts nothing about a
//! run: 7.00 for this webp arm and 3.00 for png, both from `page::decode`'s raster module.
//!
//! | arm | predicted worker | × 9 | measured peak |
//! |---|---|---|---|
//! | webp `Rgb8` | 55.93 + max(234.86, 108.42) = 290.79 MB | 2.62 GB | 2,590,294,016 B |
//! | png `Rgb8` | 55.93 + max(100.65, 108.42) = 164.35 MB | 1.48 GB | 1,387,397,120 B |
//! | jpg, progressive | not predicted | — | 1,265,893,376 B |
//!
//! The entries were 11,670 B, 444,778 B and 627,015 B, so the window term at its bound —
//! `W × MAX_ENTRY_BYTES`, 1.21 GB — is nowhere near what these archives reach: `W` times the
//! entry each actually holds is 210 KB for the webp row and 8.0 and 11.3 MB for the other two,
//! against peaks in the gigabytes. The per-worker product is nearly the whole of it. Both
//! predictions sit above their run — by 1% for webp and 6.6% for png — which is the direction a
//! bound has to miss in. The webp row is the first time this bound has been checked against a
//! run rather than asserted. jpg is not predicted because its largest term is the one this
//! formula does not charge — the progressive coefficient arrays named below.
//!
//! Neither side of the maximum is always the larger — the two predicted rows above land on one
//! each — so it is load-bearing rather than decorative. The resize side wins for the three arms
//! whose decoded buffer is *moved* into the page and whose scratch factor is one: png `L8`, png
//! `Rgb8` and bmp `Rgb8`, whose decode is the page and nothing else, against a resize holding the
//! page and the destination at once. The decode side wins for the rest, by a margin no downscale
//! closes: an arm that copies holds its declared buffer on top of the page — at least four bytes
//! a source pixel where the page is three, at least two where the page is one — and the
//! destination is smaller than the page, because [`plan`](crate::policy::plan) passes a page
//! through rather than enlarging it; and webp `Rgb8`, which is moved but allocates around its
//! buffer, holds eight bytes a source pixel against at most six.
//!
//! Which of the two *stages* is dearer, counting the resize temporary against the resize that
//! grows it rather than against the retained term, is a different question — and it is the one
//! three revisions of this paragraph answered wrongly, so the derivation is shown rather than
//! its conclusion. Counted that way the comparison is
//!
//! ```text
//! declared × factor + page   >   page + 2 × temp + destination
//! ```
//!
//! and cancelling `page` is only legal where the decode really holds the declared buffer *and*
//! the page at once — the arms that copy. **For the three factor-one arms whose buffer is moved
//! the decode is the page alone**, so nothing cancels, the right side is larger by
//! `2 × temp + destination` and the resize is the dearer stage at every ratio; the condition
//! below does not apply to them and would get them backwards. webp `Rgb8` is moved too but its
//! factor is not one, so its decode is `8/3` of the page against `page + 2 × temp + destination`
//! and it crosses like a copying arm, below `r ≈ 0.633`. For a copying arm the comparison
//! reduces to
//! `declared > 2 × temp + destination`, which at a fixed target width is a threshold on the
//! downscale ratio `r` rather than a fixed answer: with `d` the declared bytes a *source pixel*
//! — `declared` itself is `total_bytes()`, so this is `d = declared / (width × height)` — and
//! `c` the page's channels, it is `d / c > 2r + r²`, roughly `r < 0.53` for the four-byte
//! three-channel arms and `r < 0.73` for the two-byte grey ones. On the 1520x2150 page the
//! growth ratios above were measured on, `r` is 0.842 and png `Rgba8` decodes seven bytes a
//! source pixel — four declared, three composited — against `3 + 2 × 2.527 + 2.128` = 10.2 to
//! resize, so the resize is the dearer stage there, and on a source wide enough to put `r` under
//! 0.53 it is not. Each count this paragraph used to carry was taken at one geometry and written
//! as though it held at every one, and the scope above is written as part of the algebra so the
//! next reader cannot detach it again. The bound does not rest on any of this: the temporary
//! belongs to both stages, so it is charged once, outside the maximum.
//!
//! There is a fourth term this does not charge and it is libjpeg's: a **progressive** source
//! holds coefficient arrays following the source geometry whatever `scale_denom` asks for, and
//! this crate's encoder writes progressive, so the tool's own output fed back in is the
//! expensive JPEG case. Measured on one 6000x8000 page, the same picture costs 43.7 MB as a
//! baseline JPEG and 186.4 MB as a progressive one — 2.97 bytes a pixel of coefficient arrays,
//! which is 4:2:0's 1.5 samples at two bytes a coefficient. `page::budget` records it as
//! uncharged; charging it needs a progressive flag the dependency does not expose.
//!
//! What the whole thing costs on real numbers, nine workers, one archive of nine pages:
//!
//! | page | jpg | png | webp |
//! |---|---|---|---|
//! | 4800x7989, 38.3 Mpx | 1.43 GB | 1.55 GB | 2.93 GB, now refused: 306.8 MB a page |
//! | 6000x8000, 48.0 Mpx | 1.64 GB | — | refused: 384 MB a page |
//!
//! The jpg and png figures are what the machine paid with every page inside every stated
//! limit. Both webp cells are refusals: the `Rgb8` charge of eight bytes a pixel admits
//! `268,435,456 / 8` = **33,554,432 pixels**, and the `Rgba8` charge — nine bytes a pixel plus
//! the three the composite costs — admits **22,369,621**, so the first row's 38.3 Mpx no longer
//! gets in. Its 2.93 GB is what nine of those pages really did cost under the `7/3` this
//! replaced, and is the reason the two webp factors were re-derived from `image-webp`'s source
//! rather than left at the slope a ladder of `cwebp` output measured. The webp entries are
//! 13 KB each, so the second row is 118 KB of input that would have put 3.49 GB resident before
//! any of this was charged. A caller who knows the page count and the worker count still cannot
//! predict the peak; the deciding term is which decoder read the archive, and for one format it
//! is now bounded rather than merely large.
//!
//! **`J` is deliberately not bounded here.** It is the largest single factor, it is
//! `worker_count()`'s to pick, and choosing it belongs to whoever owns the command-line surface
//! — there is no `--jobs` yet, and adding one is where a default gets argued. What this module
//! owes that decision is the per-worker term above, which is why it is stated as a product
//! rather than as a number.
//!
//! # Working memory a decoder sizes from the input
//!
//! Peak memory is independent of page count, and that is what this section's claim is. It is
//! not independent of what the *input declares*, and 7z is where that shows: `lzma-rust2`
//! allocates its LZMA2 dictionary at the size the archive was written with, and the term is
//! that size. Measured end to end on 1000 identical pages, against the same pages as a zip:
//! **+33.6 MB** for an archive declaring a 32 MiB dictionary and **+103.9 MB** for one
//! declaring 96 MiB.
//!
//! The allocation is fallible, so a hostile declaration is an error rather than an abort, but
//! a merely large one is simply that many megabytes; the crate's own `MAX_MEM_LIMIT_KB` is
//! `usize::MAX / 1024`, so its guard can never fire and there is no public knob to lower it.
//!
//! An additive term, recorded here rather than folded into the page-size term it is not part
//! of. It is also per *reader*, not per worker — `SevenZSource::new` sets the decoder's
//! thread count to one, because the default is the host's parallelism and each worker holds
//! its own dictionary: 35.6 MB against 682.3 MB on the same archive.
//!
//! # \* The reader is one thread of this pipeline's making, not one OS thread
//!
//! Reading rar goes through libunrar, which is built with `RAR_SMP`: `Unpack::SetThreads`
//! does `MaxUserThreads = Min(Threads, 8)` with `Threads` from `GetNumberOfThreads()`
//! (`unpack.cpp`, `options.cpp`). So a *compressed* member can unpack across up to eight
//! further OS threads inside what this diagram calls one reader, on top of the `J` workers,
//! and the crate exposes no knob to cap it. Recorded rather than fixed: it does not affect
//! the memory bound argued above, which counts entries in flight and not threads, but a
//! comment claiming a thread count has to be true. Neither real rar sample reaches it —
//! both are entirely stored — so the peak-RSS measurement is taken on a compressed fixture
//! as well.
//!
//! Reading 7z adds one more, and this one *is* capped: `SevenZSource` runs the crate's
//! push-shaped walk on a thread of its own, and pins the LZMA2 reader to a single thread, so
//! the count is exactly one and it is this module's choice rather than the host's.

use std::num::NonZeroUsize;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::thread;

use crossbeam_channel::bounded;
use thiserror::Error;

use crate::page::{
    DecodeSettings, EncodeSettings, Filter, Format, PageError, Resampler, decode, encode, header,
};
use crate::policy::{self, Plan};
use crate::sink::{Page, PageKey, Sink};
use crate::source::{Entries, SourceError};

/// How much work may be in flight, as a multiple of the worker count.
///
/// `J` keeps every worker fed; the second `J` is the slack that lets a page complete out of
/// order without stalling the pipeline. Larger would buy nothing but memory.
const WINDOW_PER_JOB: usize = 2;

/// Every channel's capacity, derived from the worker count.
///
/// Returned as a type so the pipeline and the test that asserts each capacity is finite read
/// the same numbers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capacities {
    /// Read-ahead window: the number of entries that may exist anywhere at once.
    pub credits: usize,
    pub work: usize,
    pub done: usize,
}

impl Capacities {
    #[must_use]
    pub const fn for_jobs(jobs: NonZeroUsize) -> Self {
        let jobs = jobs.get();
        Self {
            credits: jobs * WINDOW_PER_JOB,
            work: jobs,
            done: jobs,
        }
    }
}

/// What the run is allowed to do to each page.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub jobs: NonZeroUsize,
    /// Target width for normalisation. The height follows from each page's aspect ratio.
    pub target_width: u32,
    pub filter: Filter,
    /// `scale_to` is ignored: the resize policy decides it per page.
    pub decode: DecodeSettings,
    pub encode: EncodeSettings,
}

/// What a finished run produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Report {
    pub pages: u32,
    /// How many pages carried an alpha channel that was composited onto white.
    ///
    /// **The first page outcome that is neither fine nor fatal**, and the reason this field
    /// exists rather than a warning: every page-level anomaly until now fell on one side or
    /// the other — a page libjpeg repaired by fabricating coefficients *ends the run*, because
    /// such a page is not the page — so there was no warn-and-carry channel at all. Compositing
    /// is a third kind: the tool processed the page **and altered how it looks**, where
    /// refusing it would refuse a page a viewer displays correctly and saying nothing would be
    /// the silent change this project refuses everywhere else.
    ///
    /// A count for the run rather than a line per page, because a 218-page archive would
    /// otherwise emit 218 warnings around the one line the run already prints. Zero means the
    /// caller says nothing about compositing, so a run that composited nothing produces the
    /// output it produced before this field existed.
    ///
    /// Counting is **not** a way to continue past a failure: a page that cannot be processed
    /// still ends the run, and only a page the tool processed and took a decision about is
    /// counted here.
    pub composited: u32,
}

/// Processes every page of `source` into a new archive at `output`.
///
/// # Errors
///
/// The first failure the *writer dequeues*, which with several damaged pages is whichever
/// worker finished first rather than the earliest in read order. After it, no archive is left
/// at `output` and no partial beside it.
///
/// One page that cannot be decoded, resized, or encoded ends the run: a book missing a page,
/// or holding a page the tool did not process, is worse than no output, because it is the
/// failure a reader notices last.
///
/// # Panics
///
/// If the credit channel rejects a token while it is still being filled, which cannot
/// happen: it was just created with room for exactly that many and nothing can have
/// disconnected yet.
pub fn run<S: Entries + Send>(
    source: S,
    output: &Path,
    settings: &Settings,
) -> Result<Report, RunError> {
    let capacities = Capacities::for_jobs(settings.jobs);

    let (credit_tx, credit_rx) = bounded::<()>(capacities.credits);
    for _ in 0..capacities.credits {
        credit_tx
            .send(())
            .expect("the channel was created with room for exactly this many tokens");
    }

    let (work_tx, work_rx) = bounded::<Job>(capacities.work);
    let (done_tx, done_rx) = bounded::<Result<Finished, PageError>>(capacities.done);

    // Created before any thread starts, so an existing output is refused without reading a
    // single entry.
    let mut sink = Sink::create(output)?;

    let outcome = thread::scope(|scope| {
        let reader = scope.spawn(move || {
            // Caught here rather than left to unwind out of the scope: `thread::scope`
            // re-panics for any spawned thread that panicked, which would discard the
            // payload, skip the error path below, and bypass the sink's cleanup.
            panic::catch_unwind(AssertUnwindSafe(|| {
                read_entries(source, &credit_rx, &work_tx)
            }))
            .unwrap_or(Err(RunError::StagePanicked {
                stage: "the archive reader",
            }))
        });
        for _ in 0..settings.jobs.get() {
            let work_rx = work_rx.clone();
            let done_tx = done_tx.clone();
            scope.spawn(move || {
                // One resampler per worker, so its scratch buffers are reused across pages
                // rather than rebuilt per page.
                let mut resampler = Resampler::new();
                while let Ok(job) = work_rx.recv() {
                    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                        process(job, &mut resampler, settings)
                    }))
                    .unwrap_or_else(|_| Err(PageError::stage_panicked("a page worker")));
                    if done_tx.send(outcome).is_err() {
                        break;
                    }
                }
            });
        }
        // The originals must go, or the channels never disconnect and the threads never
        // finish.
        drop(work_rx);
        drop(done_tx);

        // The writer runs here rather than on a worker: it is the one stage that must never
        // wait on another stage's thread while holding something that stage needs. It is also
        // where the composited pages are counted, because it is the one stage that sees every
        // page exactly once and in one thread.
        let mut composited = 0;
        let mut failure = None;
        while let Ok(finished) = done_rx.recv() {
            match finished {
                Ok(finished) => {
                    composited += u32::from(finished.composited);
                    match sink.accept(finished.page) {
                        Ok(written) => {
                            for _ in 0..written {
                                // The reader may already be gone; that is not a failure.
                                let _ = credit_tx.send(());
                            }
                        }
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    }
                }
                Err(error) => {
                    failure = Some(RunError::Page(error));
                    break;
                }
            }
        }

        // Disconnect both directions so the reader and the workers stop, whether the loop
        // ended because the work ran out or because a page failed.
        drop(done_rx);
        drop(credit_tx);

        let read = reader.join().unwrap_or(Err(RunError::StagePanicked {
            stage: "the archive reader",
        }));

        // A page failure is reported ahead of a reader failure: the reader's error is
        // usually just the disconnect the failure caused.
        match (failure, read) {
            (Some(error), _) | (None, Err(error)) => Err(error),
            (None, Ok(())) => Ok(composited),
        }
    });

    match outcome {
        Ok(composited) => Ok(Report {
            pages: sink.finish()?,
            composited,
        }),
        // The sink's `Drop` removes the partial; there is nothing else to undo.
        Err(error) => Err(error),
    }
}

/// One entry on its way to a worker.
struct Job {
    key: PageKey,
    name: String,
    /// The format the entry's *bytes* selected, which is what decides the decoder. The
    /// reader established it and refused a disagreement with the extension, so nothing here
    /// probes again.
    format: Format,
    bytes: Vec<u8>,
}

/// One finished page and what the run has to remember about it.
///
/// The flag rides here rather than on [`Page`] because the sink writes bytes and has no use
/// for the page's provenance; only the tally does.
struct Finished {
    page: Page,
    composited: bool,
}

/// Reads the archive once, taking a credit before each entry.
fn read_entries<S: Entries>(
    mut source: S,
    credits: &crossbeam_channel::Receiver<()>,
    work: &crossbeam_channel::Sender<Job>,
) -> Result<(), RunError> {
    loop {
        // Before reading, so an entry's bytes are never held waiting for room.
        if credits.recv().is_err() {
            return Ok(());
        }
        let Some(entry) = source.next_entry() else {
            return Ok(());
        };
        let entry = entry?;
        let job = Job {
            key: (entry.index, 0),
            name: entry.name,
            format: entry.format,
            bytes: entry.bytes,
        };
        if work.send(job).is_err() {
            return Ok(());
        }
    }
}

/// Decode, plan, resize, encode — the whole of one page.
fn process(
    job: Job,
    resampler: &mut Resampler,
    settings: &Settings,
) -> Result<Finished, PageError> {
    let Job {
        key,
        name,
        format,
        bytes,
    } = job;

    // The header first, because the resize policy needs the source geometry to choose both
    // the target height and whether to resize at all, and a scaled decode cannot be
    // configured before that is known. The budget refusal itself is inside `decode`, at the
    // point each decoder has parsed its header and before it allocates from it — for every
    // format, including the three whose decoder cannot scale. The budget is passed here
    // because reading a *png*'s header is itself an allocating operation.
    let (source_width, source_height) = header(&name, &bytes, format, settings.decode.budget)?;
    let plan = policy::plan(source_width, source_height, settings.target_width);

    let decoded = decode(
        &name,
        &bytes,
        format,
        DecodeSettings {
            scale_to: plan.scale_to(source_width, source_height),
            ..settings.decode
        },
    )?;
    // Pass-through skips the resize and nothing else.
    let page = match plan {
        Plan::Resize { width } => resampler.resize(&name, &decoded.page, width, settings.filter)?,
        Plan::PassThrough => decoded.page,
    };
    let bytes = encode(&name, &page, settings.encode)?;

    Ok(Finished {
        page: Page { key, name, bytes },
        composited: decoded.composited,
    })
}

/// Why a run stopped.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RunError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Page(#[from] PageError),
    #[error("{}: already exists", path.display())]
    OutputExists { path: PathBuf },
    /// The partial file was already there. It is created with `create_new`, so this is
    /// either a leftover from a run that died, or something placed there deliberately —
    /// possibly a link pointing somewhere else. Either way it is not overwritten.
    #[error("{}: already exists; remove it or move it aside", path.display())]
    PartialExists { path: PathBuf },
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot write the archive: {0}")]
    Archive(::zip::result::ZipError),
    /// A stage panicked. Caught rather than left to unwind out of `thread::scope`, which
    /// would discard the payload and skip the cleanup path.
    #[error("{stage} stopped unexpectedly")]
    StagePanicked { stage: &'static str },
    /// The input held no page this build can process, so there was nothing to write. An
    /// empty output would report success and then make the next run fail with "already
    /// exists".
    ///
    /// Says *input* rather than *archive* because a directory reaches it too, and a
    /// directory holding no page must not be told it is an unrecognised format.
    #[error("no pages to process: the input yielded no image entry this build can read")]
    Empty,
    /// Two stored names became one output name when their extensions were rewritten.
    #[error(
        "{name}: two entries would be written under this name once renamed to the encoder's extension"
    )]
    NameCollision { name: String },
    /// The ordering invariant broke: a page above the one being waited for was left over
    /// after every worker finished.
    #[error("page {expected} never arrived, but page {stranded} did")]
    Incomplete { expected: u32, stranded: u32 },
    /// A directory input with no name of its own — `.`, `..`, or the filesystem root — so
    /// there is nothing to derive an output name from. Reached only after the path has been
    /// resolved, so `.` is the directory the user is standing in rather than this case.
    #[error("{}: cannot name an output for a directory with no name of its own", path.display())]
    UnnamedInput { path: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::{Capacities, WINDOW_PER_JOB};
    use std::num::NonZeroUsize;

    #[test]
    fn every_channel_has_a_finite_capacity_that_scales_with_the_worker_count() {
        for jobs in [1usize, 2, 4, 8, 64] {
            let jobs = NonZeroUsize::new(jobs).expect("non-zero");
            let capacities = Capacities::for_jobs(jobs);

            // A zero-capacity crossbeam channel is a rendezvous, which would serialise the
            // pipeline; an unbounded one is what makes Go's peak scale with page count.
            for capacity in [capacities.credits, capacities.work, capacities.done] {
                assert!(capacity > 0, "{capacities:?} has a rendezvous channel");
                assert!(
                    capacity <= jobs.get() * WINDOW_PER_JOB,
                    "{capacities:?} is not bounded by the worker count"
                );
            }

            // The window has to exceed the work channel, or a page could not complete out
            // of order without stalling the reader.
            assert!(capacities.credits > capacities.work);
        }
    }
}
