// The time-based flush of a relaxed batch (spec/design/validated-cow.md §4.1).
//
// A relaxed file domain whose handle has a non-zero flushIntervalMs gives each batch a deadline on the
// handle's clock source: the reading of the batch's first commit plus the interval. A commit that finds
// the deadline passed flushes before it plans (persist.ts persistSteps); an idle batch is flushed by a
// timer on the event loop that fires when the deadline should have passed. The timer flushes only once
// the clock is past the deadline and never while one of the domain's commits is running — a commit may
// await (Node page reads, an asynchronous page codec — ts-runtime.md §11), so the timer could otherwise
// fire inside it — so it only does early what the next commit would have done and never changes what
// is written.

import type { ClockSource } from "./seam.ts";

// DEFAULT_FLUSH_INTERVAL_MS is the default flushIntervalMs: how long a synchronous = off batch may stay
// pending before it is flushed (validated-cow.md §4.1). 0 turns the time-based flush off.
export const DEFAULT_FLUSH_INTERVAL_MS = 1000;

// The deadline saturates at the largest i64 micros, as in every core.
const MAX_MICROS = (1n << 63n) - 1n;
// A setTimeout delay above 2^31 - 1 ms fires at once; a longer wait re-arms instead.
const MAX_TIMER_MS = 0x7fff_ffff;
// The idle flusher retries a busy writer gate after at most this many micros (validated-cow.md §4.1
// rule 5), as Rust's flusher thread does.
const GATE_RETRY_MICROS = 50_000n;

// Durability is a handle's durability settings as one domain passes them to another (a file
// attachment follows main's): synchronous = off, the time-based flush interval, and whether an idle
// flusher runs (false in the deterministic simulator, which moves time only on its clock steps).
export type Durability = { relaxed: boolean; flushIntervalMs: number; idleFlush: boolean };

// IdleTarget is the domain the idle flusher flushes, read when the timer fires: pending reports a batch
// still pending on usable storage; flush takes the domain's writer gate and, if begin then agrees, runs
// the cap flush (its failure revokes the storage); it returns false, flushing nothing, when the gate is
// busy.
export type IdleTarget = { pending(): boolean; flush(begin: () => boolean): boolean };

// DeadlineSlot is where a page-service owner keeps its batch deadline, so the service, which may run on
// another thread, applies rule 4 to the batch its relaxed clients plan against and commit into
// (validated-cow.md §4.1; pageservice/shared_state.ts). bind publishes the interval (micros, 0n: off)
// and whether the owner's clock is the wall clock, which the service can then read itself.
export interface DeadlineSlot {
  deadline(): bigint | null;
  setDeadline(deadline: bigint | null): void;
  bindTiming(interval: bigint, wallClock: boolean): void;
}

// saturatingDeadline is now plus interval, saturating at the largest i64 micros (rule 2).
export function saturatingDeadline(now: bigint, interval: bigint): bigint {
  const deadline = now + interval;
  return deadline > MAX_MICROS ? MAX_MICROS : deadline;
}

// BatchDeadline is one domain's time-based flush settings and batch-deadline state, shared by its
// commits and its idle flusher. The default (interval 0) is off.
export class BatchDeadline {
  // interval is flushIntervalMs in micros (0n: off).
  readonly interval: bigint;
  readonly intervalMs: number;
  // idle is whether the idle flusher runs (false: the simulator).
  readonly idle: boolean;
  // local is the current batch's deadline on the clock, in micros (null when no batch is timed), unless
  // shared holds it: a page-service owner's, which its service reads and writes too.
  private local: bigint | null = null;
  private shared: DeadlineSlot | null = null;
  // committing is set while one of the domain's commits runs, from its clock reading to its publish;
  // fireAfterCommit records a timer that fired meanwhile, so the check runs once the commit ends.
  private committing = false;
  private fireAfterCommit = false;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private target: IdleTarget | null = null;
  private clock: ClockSource | null = null;
  private stopped = false;

  constructor(flushIntervalMs = 0, idle = true) {
    const ms = Number.isFinite(flushIntervalMs) && flushIntervalMs > 0 ? flushIntervalMs : 0;
    this.intervalMs = Math.floor(Math.min(ms, Number.MAX_SAFE_INTEGER));
    const micros = BigInt(this.intervalMs) * 1000n;
    this.interval = micros > MAX_MICROS ? MAX_MICROS : micros;
    this.idle = idle;
  }

  // timed reports whether the time-based flush is on.
  timed(): boolean {
    return this.interval > 0n;
  }

  private get deadline(): bigint | null {
    return this.shared === null ? this.local : this.shared.deadline();
  }

  private set deadline(deadline: bigint | null) {
    if (this.shared === null) this.local = deadline;
    else this.shared.setDeadline(deadline);
  }

  // bind keeps the deadline in slot from now on (a page-service owner's shared state), publishing the
  // interval there and whether clock is the wall clock.
  bind(slot: DeadlineSlot, wallClock: boolean): void {
    slot.bindTiming(this.interval, wallClock);
    slot.setDeadline(this.deadline);
    this.shared = slot;
  }

  // slot is the shared slot the deadline is kept in (null: none), which a new setting keeps.
  slot(): DeadlineSlot | null {
    return this.shared;
  }

  // enter starts a commit of the domain: the idle flusher waits for it (rule 5).
  enter(): void {
    this.committing = true;
  }

  // leave ends the commit; a timer that fired during it checks the batch now.
  leave(): void {
    this.committing = false;
    if (!this.fireAfterCommit) return;
    this.fireAfterCommit = false;
    if (this.timer === null && this.deadline !== null) this.schedule(0n);
  }

  // passed reports whether a reading at now ends the pending batch (rule 3).
  passed(now: bigint): boolean {
    return this.deadline !== null && now >= this.deadline;
  }

  // clear records that another flush ended the batch.
  clear(): void {
    this.deadline = null;
  }

  // afterCommit records a commit's effect on the batch (rules 2 and 5): beganEmpty when it began with
  // nothing pending, continues when a batch is still pending after it (no cap flush followed). A commit
  // that starts a batch sets its deadline and arms the idle flusher over target and clock for the whole
  // interval, with no clock read.
  afterCommit(
    beganEmpty: boolean,
    continues: boolean,
    now: bigint,
    target: IdleTarget,
    clock: ClockSource,
  ): void {
    if (!continues) {
      this.deadline = null;
      return;
    }
    if (!beganEmpty) return;
    this.deadline = saturatingDeadline(now, this.interval);
    if (!this.idle || this.stopped) return;
    this.target = target;
    this.clock = clock;
    this.schedule(this.interval);
  }

  // armClientBatch arms the idle flusher for a batch a relaxed page client's commit started on the
  // owner's service (rule 4), which set the deadline from its reading of the owner's clock; a service
  // that could not read that clock (one on another thread, over a host clock source) left it unset, and
  // this reading sets it. Nothing is armed once the batch is gone.
  armClientBatch(target: IdleTarget, clock: ClockSource): void {
    if (!this.timed() || this.stopped) return;
    let deadline = this.deadline;
    // The simulator's service set the deadline and runs no idle flusher: nothing to read or arm.
    if (deadline !== null && !this.idle) return;
    if (!target.pending()) return;
    const now = clock.nowMicros();
    if (deadline === null) {
      deadline = saturatingDeadline(now, this.interval);
      this.deadline = deadline;
    }
    if (!this.idle) return;
    this.target = target;
    this.clock = clock;
    if (this.timer !== null) return; // the running timer sees the deadline
    if (this.committing) {
      this.fireAfterCommit = true; // checked once the commit ends
      return;
    }
    this.schedule(deadline > now ? deadline - now : 0n);
  }

  // stop ends the idle flusher (close, detach, a role change): no timer fires again.
  stop(): void {
    this.stopped = true;
    this.local = null;
    this.target = null;
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
  }

  // schedule (re)arms the timer to fire after micros (at least 1 ms once a wait is due). The timer is
  // unref'd where the host allows it, so it never keeps a Node process alive; browsers have no unref.
  private schedule(micros: bigint): void {
    if (this.timer !== null) clearTimeout(this.timer);
    const ms = micros <= 0n ? 0 : Number(micros / 1000n);
    const delay = micros <= 0n ? 0 : Math.min(Math.max(ms, 1), MAX_TIMER_MS);
    this.timer = setTimeout(() => this.fire(), delay);
    (this.timer as { unref?: () => void }).unref?.();
  }

  // fire is the idle flusher's wake: flush the batch if the clock has passed its deadline, otherwise
  // wait again for the remaining time (at most one interval).
  private fire(): void {
    this.timer = null;
    const target = this.target;
    const clock = this.clock;
    if (this.stopped || target === null || clock === null || this.deadline === null) return;
    if (this.committing) {
      this.fireAfterCommit = true;
      return;
    }
    if (!target.pending()) {
      // A shared deadline is left to the service, which may be starting the next batch meanwhile; a
      // stale one is replaced by whatever starts the next batch.
      if (this.shared === null) this.deadline = null;
      return;
    }
    const now = clock.nowMicros();
    const deadline = this.deadline;
    if (now < deadline) {
      const remaining = deadline - now;
      this.schedule(remaining < this.interval ? remaining : this.interval);
      return;
    }
    // The deadline ends only once the gate is held, and only if no flush or new batch replaced it
    // before then: a service applying rule 4 under the gate reads it.
    const begin = (): boolean => {
      if (this.deadline !== deadline) return false;
      this.clear();
      return true;
    };
    if (!target.flush(begin)) {
      // The writer gate is busy (a page-service owner's transaction, or a relaxed client planning
      // against the batch): the batch stays pending, and whoever holds the gate flushes it first or the
      // timer tries again soon (rule 5).
      this.schedule(this.interval < GATE_RETRY_MICROS ? this.interval : GATE_RETRY_MICROS);
    } else if (this.deadline !== null && this.timer === null) {
      // The batch changed before the gate was taken: wait for its deadline.
      this.schedule(0n);
    }
  }
}
