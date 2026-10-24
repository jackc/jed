//! The time-based flush of a relaxed batch (spec/design/validated-cow.md §4.1).
//!
//! A relaxed file domain whose handle has a non-zero `flush_interval_ms` gives each batch a
//! **deadline** on the handle's clock source: the reading of the batch's first commit plus the
//! interval. A commit that finds the deadline passed flushes before it plans; an idle batch is
//! flushed by a background thread that wakes when the deadline should have passed. The thread
//! flushes only once the clock is past the deadline, under the same lock every commit of the domain
//! holds from its reading to its publish, so it only does early what the next commit would have done
//! and never changes what is written. On a page-service owner the thread also takes the writer gate,
//! so it never flushes the batch a relaxed page client is planning against.

use crate::environment::Environment;
use crate::paging::SharedPaging;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// Take a page-service owner's writer gate if it is free, returning its release (validated-cow.md
/// §4.1 rule 5).
pub(crate) type FlushGate = Arc<dyn Fn() -> Option<Box<dyn FnOnce() + Send>> + Send + Sync>;

/// One domain's batch-deadline state, shared by its commits and its idle flusher.
#[derive(Default)]
pub(crate) struct Batch {
    /// The current batch's deadline on the clock, in micros; `None` when no batch is timed.
    deadline: Option<i64>,
    /// Set when a commit starts a batch: the flusher's next wait is the whole interval, with no
    /// clock read.
    armed: bool,
    /// Set when the domain's storage is dropped: the flusher exits.
    stopped: bool,
    /// Whether the flusher thread runs.
    started: bool,
    /// The paging of the batch the flusher flushes: the domain's current one when the batch began
    /// (compaction, a rekey, or a page-service role change replaces it, and ends the batch).
    paging: Option<Arc<SharedPaging>>,
    /// The owner's writer gate the flusher takes first, on a page-service owner.
    gate: Option<FlushGate>,
}

impl Batch {
    /// Any other flush ended the batch.
    pub(crate) fn clear(&mut self) {
        self.deadline = None;
        self.paging = None;
    }

    /// Whether a reading at `now` ends the pending batch (rule 3).
    pub(crate) fn passed(&self, now: i64) -> bool {
        self.deadline.is_some_and(|d| now >= d)
    }
}

struct Control {
    batch: Mutex<Batch>,
    wake: Condvar,
}

/// The owner's token: dropping the last storage clone that holds it stops the flusher.
struct Token(Arc<Control>);

impl Drop for Token {
    fn drop(&mut self) {
        let mut batch = self.0.batch.lock().unwrap_or_else(|e| e.into_inner());
        batch.stopped = true;
        self.0.wake.notify_all();
    }
}

/// A storage's time-based flush settings and state. The default (interval 0) is off.
#[derive(Clone, Default)]
pub(crate) struct DeadlineFlush {
    /// The interval in micros; 0 turns the time-based flush off.
    interval: i64,
    clock: Option<Environment>,
    token: Option<Arc<Token>>,
    gate: Option<FlushGate>,
}

impl std::fmt::Debug for DeadlineFlush {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeadlineFlush")
            .field("interval", &self.interval)
            .finish()
    }
}

/// The deadline lock a commit holds from its clock reading to its publish.
pub(crate) struct Held {
    control: Arc<Control>,
}

impl Held {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Batch> {
        self.control.batch.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl DeadlineFlush {
    /// Settings for `flush_interval_ms` measured on `env`'s clock.
    pub(crate) fn new(flush_interval_ms: u64, env: &Environment) -> Self {
        DeadlineFlush {
            interval: i64::try_from(flush_interval_ms.saturating_mul(1_000)).unwrap_or(i64::MAX),
            clock: Some(env.clone()),
            token: None,
            gate: None,
        }
    }

    /// The same settings with fresh state, for another domain of the handle (a file attachment).
    pub(crate) fn settings(&self) -> Self {
        DeadlineFlush {
            interval: self.interval,
            clock: self.clock.clone(),
            token: None,
            gate: None,
        }
    }

    /// The writer gate the idle flusher takes first: a page-service owner's, or `None`.
    pub(crate) fn set_gate(&mut self, gate: Option<FlushGate>) {
        self.gate = gate;
    }

    /// Keep the gate across a new setting of the interval.
    pub(crate) fn take_gate(&mut self) -> Option<FlushGate> {
        self.gate.take()
    }

    /// The interval in milliseconds (0: off).
    pub(crate) fn interval_ms(&self) -> u64 {
        (self.interval / 1_000) as u64
    }

    /// The deadline lock, when the time-based flush is on (creating its state on first use).
    pub(crate) fn hold(&mut self) -> Option<Held> {
        if self.interval == 0 || self.clock.is_none() {
            return None;
        }
        let token = self.token.get_or_insert_with(|| {
            Arc::new(Token(Arc::new(Control {
                batch: Mutex::new(Batch::default()),
                wake: Condvar::new(),
            })))
        });
        Some(Held {
            control: Arc::clone(&token.0),
        })
    }

    /// One reading of the handle's clock (rule 1).
    pub(crate) fn now(&self) -> i64 {
        self.clock.as_ref().map_or(0, Environment::now_micros)
    }

    /// Record a commit's effect on the batch (rules 2 and 5): `began_empty` when it began with nothing
    /// pending, `continues` when a batch is still pending after it (no cap flush followed). A commit
    /// that starts a batch sets its deadline and wakes the flusher, started over `background` (the
    /// domain's paging; `None` runs no flusher, as the simulator does).
    pub(crate) fn after_commit(
        &self,
        held: &Held,
        batch: &mut Batch,
        began_empty: bool,
        continues: bool,
        now: i64,
        background: Option<&Arc<SharedPaging>>,
    ) {
        if !continues {
            batch.clear();
            return;
        }
        if !began_empty {
            return;
        }
        batch.deadline = Some(now.saturating_add(self.interval));
        batch.armed = true;
        held.control.wake.notify_all();
        let Some(paging) = background else {
            return;
        };
        batch.paging = Some(Arc::clone(paging));
        batch.gate = self.gate.clone();
        if !batch.started {
            let control = Arc::clone(&held.control);
            let clock = self.clock.clone().expect("a timed flush has a clock");
            let interval = self.interval;
            let spawned = std::thread::Builder::new()
                .name("jed-idle-flush".into())
                .spawn(move || run(&control, &clock, interval));
            batch.started = spawned.is_ok();
        }
    }
}

/// The idle flusher: wait out the deadline, then flush if the clock has passed it.
fn run(control: &Control, clock: &Environment, interval: i64) {
    let wait = |micros: i64| Duration::from_micros(micros.max(1_000) as u64);
    let mut batch = control.batch.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if batch.stopped {
            return;
        }
        let Some(deadline) = batch.deadline else {
            batch = control.wake.wait(batch).unwrap_or_else(|e| e.into_inner());
            continue;
        };
        if batch.armed {
            batch.armed = false;
            batch = control
                .wake
                .wait_timeout(batch, wait(interval))
                .unwrap_or_else(|e| e.into_inner())
                .0;
            continue;
        }
        let now = clock.now_micros();
        if now < deadline {
            let remaining = deadline.saturating_sub(now).min(interval);
            batch = control
                .wake
                .wait_timeout(batch, wait(remaining))
                .unwrap_or_else(|e| e.into_inner())
                .0;
            continue;
        }
        // On a page-service owner, a writer holding the gate may be a relaxed client planning
        // against this batch: wait for it (its commit, or the next, ends or extends the batch).
        let release = match &batch.gate {
            Some(gate) => match gate() {
                Some(release) => Some(release),
                None => {
                    batch = control
                        .wake
                        .wait_timeout(batch, wait(interval.min(50_000)))
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                    continue;
                }
            },
            None => None,
        };
        batch.deadline = None;
        if let Some(paging) = batch.paging.take() {
            let _ = crate::shared::cap_flush(&paging);
        }
        if let Some(release) = release {
            release();
        }
    }
}
