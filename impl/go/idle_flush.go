package jed

import "time"

// The time-based flush of a relaxed batch (spec/design/validated-cow.md §4.1).
//
// A relaxed file domain whose handle has a non-zero FlushIntervalMs gives each batch a deadline on the
// handle's clock source: the reading of the batch's first commit plus the interval. A commit that
// finds the deadline passed flushes before it plans; an idle batch is flushed by a goroutine that
// wakes when the deadline should have passed. The goroutine flushes only once the clock is past the
// deadline, under storage.mu, which every commit of the domain holds from its reading to its publish,
// so it only does early what the next commit would have done and never changes what is written. On a
// page-service owner the goroutine also takes the writer gate, so it never flushes the batch a relaxed
// page client is planning against.

// deadlineFlush is one storage's time-based flush settings and batch state. Guarded by storage.mu.
// The zero value (interval 0) is off.
type deadlineFlush struct {
	// interval is the flush interval in micros; 0 turns the time-based flush off.
	interval int64
	// clock is the handle's Environment, whose clock source the readings come from.
	clock *Environment
	// deadline is the current batch's deadline in micros, meaningful only while timed is set.
	deadline int64
	timed    bool
	// armed is set when a commit starts a batch: the flusher's next wait is the whole interval, with
	// no clock read.
	armed bool
	// started is whether the flusher goroutine runs; stopped is set when the storage closes, and the
	// flusher then exits. wake (buffered) re-evaluates a waiting flusher; stop is closed at stop.
	started bool
	stopped bool
	wake    chan struct{}
	stop    chan struct{}
	// gate takes a page-service owner's writer gate if it is free, returning its release (nil when it
	// is held); nil on a domain that is not an owner (validated-cow.md §4.1 rule 5). Set by the role
	// change, so a new interval keeps it.
	gate func() func()
}

// setSynchronous sets the handle's durability mode on this domain: synchronous = off (relaxed) and
// its time-based flush of flushIntervalMs on env's clock (validated-cow.md §4.1). Called at open,
// before any commit.
func (st *storage) setSynchronous(relaxed bool, flushIntervalMs uint64, env *Environment) {
	st.relaxed = relaxed
	st.deadline.setInterval(flushIntervalMs, env)
}

// setInterval sets the settings, keeping any running flusher and the owner's gate.
func (d *deadlineFlush) setInterval(flushIntervalMs uint64, env *Environment) {
	micros := int64(1<<63 - 1)
	if flushIntervalMs <= uint64(micros)/1000 {
		micros = int64(flushIntervalMs) * 1000
	}
	d.interval = micros
	d.clock = env
	d.timed = false
}

// settings is the same settings with fresh state, for another domain of the handle (a file
// attachment).
func (d *deadlineFlush) settings() deadlineFlush {
	return deadlineFlush{interval: d.interval, clock: d.clock}
}

// intervalMs is the interval in milliseconds (0: off).
func (d *deadlineFlush) intervalMs() uint64 { return uint64(d.interval / 1000) }

// clear ends the timed batch: any other flush ended it (validated-cow.md §4.1).
func (d *deadlineFlush) clear() { d.timed = false }

// passed reports whether a reading at now ends the pending batch (rule 3).
func (d *deadlineFlush) passed(now int64) bool { return d.timed && now >= d.deadline }

// timedFlush reports whether this domain times its batches (rule 1): a file domain that is not a page
// client's storage, with a non-zero interval. The owner times every batch in its file, a synchronous
// one's included, since its relaxed page clients may open one. Caller holds st.mu.
func (st *storage) timedFlush() bool {
	return st.path != "" && !st.serviceClient && st.deadline.interval > 0 && st.deadline.clock != nil
}

// setFlushGate sets the writer gate this domain's idle flusher takes first: a page-service owner's, or
// nil (validated-cow.md §4.1 rule 5). Caller holds st.mu.
func (st *storage) setFlushGate(o *pageServiceOwner) {
	if o == nil {
		st.deadline.gate = nil
		return
	}
	st.deadline.gate = func() func() {
		if !o.tryGate() {
			return nil
		}
		return o.releaseGate
	}
}

// beginClientBatch is a relaxed page client's begin_write on the owner (validated-cow.md §4.1 rule 4):
// the client is about to plan against the pending batch, so a batch past its deadline is flushed first,
// as the owner's own commit would (shrinking nothing, like the flush before a synchronous client).
// Caller holds st.mu.
func (st *storage) beginClientBatch() error {
	if err := st.awaitFlush(); err != nil {
		return err
	}
	if !st.timedFlush() || !st.batchOpen() {
		return nil
	}
	if !st.deadline.passed(st.deadline.clock.nowMicros()) {
		return nil
	}
	if err := st.paging.withPager(func(p *pager) error { return p.flushPending(false) }); err != nil {
		return err
	}
	st.deadline.clear()
	return nil
}

// afterClientCommit records a relaxed page client's commit on the owner (validated-cow.md §4.1 rule
// 4): one that started a batch sets its deadline from a reading taken now; one that reached the cap
// ends it. Caller holds st.mu.
func (st *storage) afterClientCommit(beganEmpty, capReached bool) {
	if !st.timedFlush() {
		return
	}
	if capReached {
		st.deadline.clear()
		return
	}
	if beganEmpty {
		st.afterTimedCommit(true, true, st.deadline.clock.nowMicros())
	}
}

// afterTimedCommit records a commit's effect on the batch (rules 2 and 5): beganEmpty when it began with
// nothing pending, continues when a batch is still pending after it (no cap flush followed). A commit
// that starts a batch sets its deadline and wakes the flusher, starting it on first use unless the
// cap flush runs inline (the simulator, which runs no flusher). Caller holds st.mu.
func (st *storage) afterTimedCommit(beganEmpty, continues bool, now int64) {
	d := &st.deadline
	if !continues {
		d.timed = false
		return
	}
	if !beganEmpty {
		return
	}
	d.deadline = satAddInt64(now, d.interval)
	d.timed = true
	d.armed = true
	if st.inlineFlush || d.stopped {
		return
	}
	if !d.started {
		d.started = true
		d.wake = make(chan struct{}, 1)
		d.stop = make(chan struct{})
		go st.runIdleFlush(d.wake, d.stop)
		return
	}
	select {
	case d.wake <- struct{}{}:
	default:
	}
}

// stopIdleFlush stops the flusher (close and detach); idempotent. Caller holds st.mu.
func (st *storage) stopIdleFlush() {
	d := &st.deadline
	d.timed = false
	if d.stopped {
		return
	}
	d.stopped = true
	if d.stop != nil {
		close(d.stop)
	}
}

// runIdleFlush is the idle flusher: wait out the deadline, then flush if the clock has passed it.
// Right after a batch starts it waits the whole interval without reading the clock; afterwards it
// waits the time left, at most one interval. Its flush is the cap flush, whose failure revokes the
// storage.
func (st *storage) runIdleFlush(wake, stop chan struct{}) {
	st.mu.Lock()
	for {
		d := &st.deadline
		if d.stopped {
			st.mu.Unlock()
			return
		}
		wait := int64(-1) // no batch: wait for a wake
		if d.timed {
			if d.armed {
				d.armed = false
				wait = d.interval
			} else if now := d.clock.nowMicros(); now < d.deadline {
				wait = min(d.deadline-now, d.interval)
			} else {
				// On a page-service owner, a writer holding the gate may be a relaxed client planning
				// against this batch: wait for it (its commit, or the next, ends or extends the batch).
				// Only a try: a gate holder may be waiting for st.mu.
				var release func()
				if d.gate != nil {
					if release = d.gate(); release == nil {
						wait = min(d.interval, 50_000)
					}
				}
				if d.gate == nil || release != nil {
					d.timed = false
					if st.paging != nil && !st.serviceClient {
						_ = capFlush(st.paging)
					}
					if release != nil {
						release()
					}
					continue
				}
			}
		}
		st.mu.Unlock()
		idleWait(wait, wake, stop)
		st.mu.Lock()
	}
}

// idleWait waits micros (at least a millisecond; forever when negative), a wake, or the stop.
func idleWait(micros int64, wake, stop chan struct{}) {
	if micros < 0 {
		select {
		case <-wake:
		case <-stop:
		}
		return
	}
	timer := time.NewTimer(time.Duration(min(max(micros, 1000), int64(1<<63-1)/1000)) * time.Microsecond)
	defer timer.Stop()
	select {
	case <-timer.C:
	case <-wake:
	case <-stop:
	}
}
