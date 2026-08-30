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

use std::num::NonZeroUsize;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::thread;

use crossbeam_channel::bounded;
use thiserror::Error;

use crate::page::{
    DecodeSettings, EncodeSettings, Filter, PageError, Resampler, decode, encode, header,
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
    let (done_tx, done_rx) = bounded::<Result<Page, PageError>>(capacities.done);

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
        // wait on another stage's thread while holding something that stage needs.
        let mut failure = None;
        while let Ok(finished) = done_rx.recv() {
            match finished {
                Ok(page) => match sink.accept(page) {
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
                },
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
            (None, Ok(())) => Ok(()),
        }
    });

    match outcome {
        Ok(()) => Ok(Report {
            pages: sink.finish()?,
        }),
        // The sink's `Drop` removes the partial; there is nothing else to undo.
        Err(error) => Err(error),
    }
}

/// One entry on its way to a worker.
struct Job {
    key: PageKey,
    name: String,
    bytes: Vec<u8>,
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
            bytes: entry.bytes,
        };
        if work.send(job).is_err() {
            return Ok(());
        }
    }
}

/// Decode, plan, resize, encode — the whole of one page.
fn process(job: Job, resampler: &mut Resampler, settings: &Settings) -> Result<Page, PageError> {
    let Job { key, name, bytes } = job;

    // The header first, because the resize policy needs the source geometry to choose both
    // the target height and whether to resize at all, and a scaled decode cannot be
    // configured before that is known.
    let (source_width, source_height) = header(&name, &bytes)?;
    let plan = policy::plan(source_width, source_height, settings.target_width);

    let decoded = decode(
        &name,
        &bytes,
        DecodeSettings {
            scale_to: plan.scale_to(source_width, source_height),
            ..settings.decode
        },
    )?;
    // Pass-through skips the resize and nothing else.
    let page = match plan {
        Plan::Resize { width } => resampler.resize(&name, &decoded, width, settings.filter)?,
        Plan::PassThrough => decoded,
    };
    let bytes = encode(&name, &page, settings.encode)?;

    Ok(Page { key, name, bytes })
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
    /// The archive held no page this build can process, so there was nothing to write. An
    /// empty output would report success and then make the next run fail with "already
    /// exists".
    #[error("no pages to process: the archive holds no image entry this build can read")]
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
