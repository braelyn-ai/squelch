//! An [`Embedder`] that puts the ONNX session down when nobody is asking.
//!
//! THE SESSION IS THE DAEMON. On a hosted tenant's pod the whole Rust side,
//! SQLite included, is 25-40 MB; the fastembed/ONNX session is 250-300 MB once
//! the pod has embedded anything, which is 85-90% of the pod. And a mailbox is
//! idle almost all the time: a poll tick every few minutes ingests a handful of
//! messages, embeds them in well under a second, and then the session sits there
//! holding a quarter of a gigabyte until the process restarts.
//!
//! So it does not sit there. [`LazyEmbedder`] loads once at construction (which
//! is what keeps readiness honest: an unknown model name, a dimension that
//! disagrees with the vec0 table, or a failed first-run download still fails at
//! startup, exactly as [`super::FastEmbedder`] does), and after that a reaper
//! calls [`LazyEmbedder::reap_if_idle`] on a timer. Past the idle window the
//! session is dropped and the heap is handed back with [`crate::mem::trim`],
//! which is the half that actually moves RSS. The next embed or search reloads
//! it from the on-disk weights in about 200 ms.
//!
//! WHAT THIS COSTS: one search, or one poll tick that ingested mail, pays ~200 ms
//! extra if it is the first after an idle stretch. Nothing else changes.
//! Serialization is what it always was: one session, one mutex, every call
//! already under `spawn_blocking`.

use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use super::{EmbedSettings, Embedder};
use crate::error::{CoreError, Result};
use crate::metrics::SyncMetrics;

/// One loaded embedding session, as [`LazyEmbedder`] sees it. Narrow on purpose:
/// the wrapper does lifecycle (load, stamp, drop) and nothing else, so the whole
/// of it is testable against a fake session with no weights on disk and no
/// network. The real implementation is on fastembed's `TextEmbedding`, in
/// [`super::fastembed_impl`].
///
/// `&mut self` because fastembed's does, which is why callers reach a session
/// through the mutex rather than sharing it.
pub(super) trait EmbedSession: Send {
    fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// How a session gets built. A field rather than a hardcoded call so the tests
/// can hand over a fake one; production always passes
/// [`super::fastembed_impl::load_boxed_session`].
type SessionLoader = Box<dyn Fn(&EmbedSettings) -> Result<Box<dyn EmbedSession>> + Send + Sync>;

/// The session and when it was last touched, together under one lock. They HAVE
/// to be one lock: a reaper that read the stamp, decided "idle", and then took
/// the session could drop it out from under an embed that started in between.
struct State {
    session: Option<Box<dyn EmbedSession>>,
    /// Monotonic, so a clock adjustment on the box cannot make a busy embedder
    /// look idle for an hour (or a dormant one look busy forever).
    last_used: Instant,
    /// A session was dropped on the poison-recovery path, where the guard is
    /// handed back to a caller and nothing trims. The next reap trims for it,
    /// so the arenas do not sit stranded behind `session.is_none()` forever.
    untrimmed_drop: bool,
}

/// What one [`LazyEmbedder::reap_if_idle`] found. Three states rather than a
/// bool because the reaper treats them differently: an unload gets a log line,
/// a keep is the normal answer, and CONTENTION is neither — one contended tick
/// is an embedder doing its job, and a long run of them is a session no reaper
/// can ever reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapOutcome {
    /// The session was dropped and the heap handed back.
    Unloaded,
    /// Nothing to do: still inside the idle window, or already unloaded.
    Kept,
    /// Somebody held the lock, so this is by definition not an idle embedder.
    Contended,
}

/// An [`Embedder`] that unloads its session after an idle stretch and reloads on
/// the next call. See the module docs for the why and the numbers.
pub struct LazyEmbedder {
    settings: EmbedSettings,
    /// Cached off `settings` so [`Embedder::dims`] can answer with no session
    /// loaded. The store asks this at attach time and search asks it per query;
    /// neither should be a reason to page 250 MB back in.
    dims: usize,
    loader: SessionLoader,
    /// Optional, and only ever written: the gauge that lets an operator line the
    /// memory sawtooth up against what the daemon was doing. A daemon with no
    /// metrics door (`squelchd sync`) passes `None`.
    metrics: Option<Arc<SyncMetrics>>,
    state: Mutex<State>,
}

impl LazyEmbedder {
    /// Build from resolved [`EmbedSettings`], LOADING THE SESSION EAGERLY. The
    /// eagerness is deliberate: construction is where an unknown model, a
    /// dimension mismatch, or a first-run download failure has to surface, and
    /// deferring that would turn a startup error into a mystery search failure
    /// an hour later. Readiness semantics are therefore unchanged from
    /// [`super::FastEmbedder`]: the same load, at the same moment.
    pub fn new(settings: &EmbedSettings) -> Result<Self> {
        Self::with_loader(
            settings.clone(),
            Box::new(super::fastembed_impl::load_boxed_session),
        )
    }

    /// Attach the registry the loaded/unloaded gauge is written to. Stamps the
    /// current state at once, so a scrape between here and the first reap does
    /// not report an unloaded session that is sitting right there.
    pub fn with_metrics(mut self, metrics: Arc<SyncMetrics>) -> Self {
        let loaded = self.is_loaded();
        self.metrics = Some(metrics);
        self.note_loaded(loaded);
        self
    }

    /// The shared constructor. Private, because the loader seam exists for the
    /// tests and not for callers.
    fn with_loader(settings: EmbedSettings, loader: SessionLoader) -> Result<Self> {
        let dims = settings.dims;
        let session = loader(&settings)?;
        Ok(Self {
            settings,
            dims,
            loader,
            metrics: None,
            state: Mutex::new(State {
                session: Some(session),
                last_used: Instant::now(),
                untrimmed_drop: false,
            }),
        })
    }

    /// Take the lock, RECOVERING FROM POISON instead of propagating it.
    ///
    /// A panic inside a session poisons the mutex, and every reflex answer to
    /// that is wrong here. Propagating it makes the embedder permanently
    /// useless; reading it as "no session" leaves ~250 MB of ONNX arenas
    /// resident for the life of the process with nothing able to reach them.
    /// The session is throwaway by construction, so a poisoned one is DROPPED
    /// and the next call reloads a clean one.
    fn lock_state(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                self.state.clear_poison();
                let mut state = poisoned.into_inner();
                if state.session.take().is_some() {
                    // Under the lock, for the reason [`Self::reap_if_idle`]
                    // gives.
                    self.note_loaded(false);
                    state.untrimmed_drop = true;
                }
                state
            }
        }
    }

    /// Whether the session is resident right now. Diagnostics and tests; the
    /// embed path never asks, it just loads what it needs.
    pub fn is_loaded(&self) -> bool {
        self.lock_state().session.is_some()
    }

    /// Drop the session if it has gone `max_idle` without a call, and hand the
    /// heap back. The outcome is the caller's cue to log a line, or to notice
    /// that it has not been able to look at all.
    ///
    /// `Duration::ZERO` reaps unconditionally. "Never unload" is a decision for
    /// the caller (`[embed] idle_unload_secs = 0` simply never spawns a reaper),
    /// not a special case in here.
    ///
    /// Contention counts as use: if an embed holds the lock, this is by
    /// definition not an idle embedder, so the reaper leaves and comes back on
    /// the next tick rather than waiting behind a batch.
    ///
    /// Blocking-ish: dropping an ONNX session and trimming the arenas are both
    /// millisecond-scale, so callers on a runtime should come through
    /// `spawn_blocking`.
    pub fn reap_if_idle(&self, max_idle: Duration) -> ReapOutcome {
        let (mut state, poisoned) = match self.state.try_lock() {
            Ok(state) => (state, false),
            // POISON IS NOT CONTENTION, and `try_lock` reports both through one
            // `Err`. Collapsing them would pin a panicked session forever:
            // nobody holds that lock, so no later tick would get a better
            // answer, and the reaper would leave 250 MB of unreachable arenas
            // in place while `is_loaded` said false.
            Err(TryLockError::Poisoned(e)) => {
                self.state.clear_poison();
                (e.into_inner(), true)
            }
            Err(TryLockError::WouldBlock) => return ReapOutcome::Contended,
        };
        if state.session.is_none() {
            if state.untrimmed_drop {
                state.untrimmed_drop = false;
                drop(state);
                crate::mem::trim();
            }
            return ReapOutcome::Kept;
        }
        // The idle window does not apply to a session a panic went through: it
        // is not idle, it is broken, and it costs the same as a useful one.
        if !poisoned && state.last_used.elapsed() < max_idle {
            return ReapOutcome::Kept;
        }
        // Dropped while the lock is held, so no call can be mid-embed on it.
        state.session = None;
        // Written UNDER THE LOCK, and that is the whole point: with the gauge
        // set after the guard dropped, a reload landing in between would set 1
        // and then have this overwrite it with 0, leaving the series reading
        // "unloaded" for a session sitting right there.
        self.note_loaded(false);
        drop(state);
        // THE HALF THAT MOVES RSS. Without it the drop above frees ~400 MB that
        // glibc keeps for itself and the pod's memory graph does not budge; see
        // [`crate::mem`].
        crate::mem::trim();
        ReapOutcome::Unloaded
    }

    /// Record the session's residency on the gauge. A no-op with no registry.
    fn note_loaded(&self, loaded: bool) {
        if let Some(m) = &self.metrics {
            m.set_embedder_loaded(loaded);
        }
    }
}

impl Embedder for LazyEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.embed_batch(std::slice::from_ref(&text.to_string()))?;
        out.pop()
            .ok_or_else(|| CoreError::Other(anyhow::anyhow!("embedder returned no vector")))
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        // BEFORE the lock and before any load: an empty batch is not a use, and
        // making it one would let a caller that embeds nothing keep 250 MB alive.
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // Poison recovers rather than erroring: a session a panic went through
        // is thrown away here and rebuilt below, which is the same thing the
        // reaper does with one. See [`Self::lock_state`].
        let mut state = self.lock_state();

        if state.session.is_none() {
            let started = Instant::now();
            // A failed reload leaves the embedder unloaded rather than half-set:
            // the error goes back to the caller, which handles it the same way it
            // handles any other embed failure (ingest defers to the vector
            // backfill, a search surfaces it), and the next call retries.
            let session = (self.loader)(&self.settings)?;
            state.session = Some(session);
            state.untrimmed_drop = false;
            // Still holding the guard, so this cannot be raced by a reap
            // deciding the same instant that nothing is loaded.
            self.note_loaded(true);
            eprintln!(
                "squelch: embedder reloaded in {} ms (model {}, weights were already on disk)",
                started.elapsed().as_millis(),
                self.settings.model_name
            );
        }

        let session = state
            .session
            .as_mut()
            .expect("session was present or was just loaded");
        let out = session.embed_batch(texts);
        // Stamped on the way out, not the way in: a batch that takes two seconds
        // was in use for those two seconds, and the idle window should start
        // when it finished. Stamped on failure too, so a session that is erroring
        // is not also being torn down and rebuilt under the caller.
        state.last_used = Instant::now();
        out
    }

    fn dims(&self) -> usize {
        self.dims
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A session with no weights, no ONNX, and no network. Its "embedding" is a
    /// one-hot of the text length, which is enough to prove a call reached a
    /// live session.
    struct FakeSession {
        dims: usize,
    }

    impl EmbedSession for FakeSession {
        fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0.0f32; self.dims];
                    v[0] = t.len() as f32;
                    v
                })
                .collect())
        }
    }

    /// A session that panics instead of embedding. `embed_batch` runs with the
    /// state guard held, so this is exactly how a real one poisons the mutex.
    struct PanickingSession;

    impl EmbedSession for PanickingSession {
        fn embed_batch(&mut self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
            panic!("ort blew up mid-batch");
        }
    }

    /// A session that parks inside `embed_batch` until the test lets it go,
    /// which is how "an embed holds the lock" gets tested with no sleeps: the
    /// reaper's `try_lock` happens between the two channel operations.
    struct BlockingSession {
        dims: usize,
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl EmbedSession for BlockingSession {
        fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.entered.send(()).expect("the test is listening");
            self.release.recv().expect("the test releases it");
            Ok(vec![vec![0.0f32; self.dims]; texts.len()])
        }
    }

    /// The settings every test here builds from. Nothing reads the paths: the
    /// fake loaders never touch a disk.
    fn fake_settings() -> EmbedSettings {
        EmbedSettings {
            model_name: "fake-model".to_string(),
            dims: 4,
            cache_dir: PathBuf::from("/nonexistent"),
            max_tokens: 256,
        }
    }

    /// A lazy embedder over [`FakeSession`], plus the counter its loader bumps.
    /// The counter IS the assertion in most of these tests: "did it reload" is
    /// not observable from the vectors.
    fn fake_embedder() -> (LazyEmbedder, Arc<AtomicUsize>) {
        let loads = Arc::new(AtomicUsize::new(0));
        let counter = loads.clone();
        let settings = fake_settings();
        let embedder = LazyEmbedder::with_loader(
            settings,
            Box::new(move |s: &EmbedSettings| {
                counter.fetch_add(1, Ordering::Relaxed);
                Ok(Box::new(FakeSession { dims: s.dims }) as Box<dyn EmbedSession>)
            }),
        )
        .expect("the fake loader cannot fail");
        (embedder, loads)
    }

    #[test]
    fn construction_loads_exactly_once_and_embedding_does_not_reload() {
        let (e, loads) = fake_embedder();
        assert_eq!(
            loads.load(Ordering::Relaxed),
            1,
            "eager load at construction"
        );
        assert!(e.is_loaded());

        let v = e.embed("hello").expect("embed");
        assert_eq!(v.len(), 4);
        assert_eq!(v[0], 5.0);
        assert_eq!(
            loads.load(Ordering::Relaxed),
            1,
            "a session already in hand is not rebuilt"
        );
    }

    #[test]
    fn a_reap_inside_the_idle_window_is_a_no_op() {
        let (e, loads) = fake_embedder();
        assert_eq!(e.reap_if_idle(Duration::from_secs(3600)), ReapOutcome::Kept);
        assert!(e.is_loaded());
        assert_eq!(loads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_reap_past_the_idle_window_unloads() {
        let (e, loads) = fake_embedder();
        assert_eq!(
            e.reap_if_idle(Duration::ZERO),
            ReapOutcome::Unloaded,
            "reported the unload"
        );
        assert!(!e.is_loaded());
        assert_eq!(loads.load(Ordering::Relaxed), 1, "unloading builds nothing");
    }

    #[test]
    fn reaping_an_already_unloaded_embedder_reports_nothing() {
        let (e, _loads) = fake_embedder();
        assert_eq!(e.reap_if_idle(Duration::ZERO), ReapOutcome::Unloaded);
        // The second call has nothing to drop, so it must not claim it did: the
        // caller logs an unload line off this, once a minute, forever.
        assert_eq!(e.reap_if_idle(Duration::ZERO), ReapOutcome::Kept);
    }

    #[test]
    fn the_next_embed_after_a_reap_reloads() {
        let (e, loads) = fake_embedder();
        assert_eq!(e.reap_if_idle(Duration::ZERO), ReapOutcome::Unloaded);

        let v = e.embed("hi").expect("embed after unload");
        assert_eq!(v[0], 2.0, "the reloaded session answered");
        assert_eq!(loads.load(Ordering::Relaxed), 2, "one reload, not more");
        assert!(e.is_loaded());

        // And it stays: a reload is a use, so the freshly loaded session is not
        // eligible again until the window passes.
        assert_eq!(e.reap_if_idle(Duration::from_secs(3600)), ReapOutcome::Kept);
        assert_eq!(loads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn dims_answers_with_no_session_loaded() {
        let (e, loads) = fake_embedder();
        assert_eq!(e.reap_if_idle(Duration::ZERO), ReapOutcome::Unloaded);
        // The store asks this at attach time and search asks per query; paging
        // the model back in to answer "384" would defeat the whole exercise.
        assert_eq!(e.dims(), 4);
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert!(!e.is_loaded());
    }

    #[test]
    fn an_empty_batch_neither_loads_nor_counts_as_use() {
        let (e, loads) = fake_embedder();
        assert_eq!(e.reap_if_idle(Duration::ZERO), ReapOutcome::Unloaded);
        assert!(e.embed_batch(&[]).expect("empty batch").is_empty());
        assert_eq!(loads.load(Ordering::Relaxed), 1, "nothing was loaded");
        assert!(!e.is_loaded());
    }

    #[test]
    fn a_failed_reload_leaves_the_embedder_unloaded_and_retryable() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let settings = fake_settings();
        let e = LazyEmbedder::with_loader(
            settings,
            Box::new(move |s: &EmbedSettings| {
                // Works once (construction), then refuses: the shape of a box
                // whose weights got evicted from disk under it.
                if counter.fetch_add(1, Ordering::Relaxed) == 0 {
                    Ok(Box::new(FakeSession { dims: s.dims }) as Box<dyn EmbedSession>)
                } else {
                    Err(CoreError::Other(anyhow::anyhow!("weights gone")))
                }
            }),
        )
        .expect("first load");

        assert_eq!(e.reap_if_idle(Duration::ZERO), ReapOutcome::Unloaded);
        assert!(e.embed("hello").is_err(), "the reload failure surfaces");
        assert!(!e.is_loaded(), "and nothing half-loaded was left behind");
        assert!(e.embed("hello").is_err(), "the next call retries");
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_poisoned_lock_still_unloads_and_the_next_embed_reloads() {
        let loads = Arc::new(AtomicUsize::new(0));
        let counter = loads.clone();
        let e = LazyEmbedder::with_loader(
            fake_settings(),
            Box::new(move |s: &EmbedSettings| {
                // The first session panics; every one after it behaves, which
                // is the whole claim: a poisoned session is replaceable.
                if counter.fetch_add(1, Ordering::Relaxed) == 0 {
                    Ok(Box::new(PanickingSession) as Box<dyn EmbedSession>)
                } else {
                    Ok(Box::new(FakeSession { dims: s.dims }) as Box<dyn EmbedSession>)
                }
            }),
        )
        .expect("first load");

        let hush = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.embed("boom")));
        std::panic::set_hook(hush);
        assert!(outcome.is_err(), "the session panicked under the lock");

        // THE BUG THIS GUARDS: `try_lock` reports Poisoned and WouldBlock
        // through one `Err`, so reading both as "busy" would pin ~250 MB for
        // the life of the process. Nobody holds this lock, and the window here
        // is an hour, which a broken session does not get to sit out.
        assert_eq!(
            e.reap_if_idle(Duration::from_secs(3600)),
            ReapOutcome::Unloaded
        );
        assert!(!e.is_loaded());

        let v = e.embed("hi").expect("a clean session after the poison");
        assert_eq!(v[0], 2.0, "the reloaded session answered");
        assert_eq!(loads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_reap_while_an_embed_holds_the_lock_is_contention_not_an_unload() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        // The loader is `Fn`, so the one session it hands out lives in a cell
        // it takes from.
        let pending = Mutex::new(Some(BlockingSession {
            dims: 4,
            entered: entered_tx,
            release: release_rx,
        }));
        let e = Arc::new(
            LazyEmbedder::with_loader(
                fake_settings(),
                Box::new(move |_s: &EmbedSettings| {
                    let session = pending.lock().expect("no panics in here").take();
                    Ok(Box::new(session.expect("loaded exactly once")) as Box<dyn EmbedSession>)
                }),
            )
            .expect("first load"),
        );

        let worker = {
            let e = e.clone();
            std::thread::spawn(move || e.embed("hold the lock").expect("embed"))
        };
        entered_rx.recv().expect("the session took the lock");

        // Past the window by every measure, and it must STILL not unload:
        // waiting behind a batch is the one thing the reaper must not do, and
        // dropping a session mid-embed is the one thing it cannot do.
        assert_eq!(e.reap_if_idle(Duration::ZERO), ReapOutcome::Contended);

        release_tx.send(()).expect("release the embed");
        worker.join().expect("the embed finished");
        assert!(e.is_loaded(), "contention left the session where it was");
    }
}
