package jed

// Shared test infrastructure for the crash / corruption families (.scratch/testing-ideas.md §1
// items 3 & 4, the Tier-1 durability attacks). Two pieces other test files build on:
//
//   - an in-memory blockStore (sliceStore) + a recording decorator (recordingStore) — the
//     "BlockStore-decorator for torn-write atomicity" the ideas doc names. A real commit driven
//     through the recording store yields the exact ordered (write/sync/setSize) log a power loss
//     could interrupt, so a crash is replayed deterministically (applyCrash) rather than needing a
//     real kill -9. The whole sweep stays in memory: fast, reproducible, no temp files.
//   - openImage / guardedScanIDs — open an arbitrary on-disk image and read it back under a panic
//     guard + a time budget, so the intrinsic oracle (never crash, never loop, fail closed) is
//     mechanical: a panic or a hang becomes a non-EngineError, which the callers reject.
//
// These are the Go carve-out for the crash/corruption oracle (testing-ideas.md §3): the oracle is
// intrinsic (don't crash, don't loop, fail closed), so — unlike the differential corpus — it does
// not need the other cores. It lives entirely in package-internal test code.

import (
	"errors"
	"fmt"
	"io"
	"time"
)

// sliceStore is an in-memory blockStore backed by a growable byte slice — a faithful, fast stand-in
// for fileBlockStore that mirrors its observable semantics (a short read past size() is a host
// error; writeAt/setSize grow with zero fill). No fsync to do, so sync() is a no-op. Used both to
// drive a recorded commit and to open a reconstructed crash/corruption image without touching disk.
type sliceStore struct {
	buf []byte
}

// newSliceStore copies image so the caller's buffer stays pristine across a replay sweep.
func newSliceStore(image []byte) *sliceStore {
	b := make([]byte, len(image))
	copy(b, image)
	return &sliceStore{buf: b}
}

func (s *sliceStore) readAt(off int64, length int) ([]byte, error) {
	// Mirror fileBlockStore.readAt / os.File.ReadAt: any short read (past the current length) is an
	// error, never a half-filled buffer — so a truncated image fails closed (58030) on the missing page.
	if off < 0 || length < 0 || off+int64(length) > int64(len(s.buf)) {
		return nil, ioError(io.ErrUnexpectedEOF)
	}
	out := make([]byte, length)
	copy(out, s.buf[off:off+int64(length)])
	return out, nil
}

func (s *sliceStore) writeAt(off int64, p []byte) error {
	if off < 0 {
		return ioError(errors.New("negative offset"))
	}
	end := off + int64(len(p))
	if end > int64(len(s.buf)) { // pwrite past EOF grows the file (os.File semantics)
		s.buf = append(s.buf, make([]byte, end-int64(len(s.buf)))...)
	}
	copy(s.buf[off:end], p)
	return nil
}

func (s *sliceStore) sync() error { return nil } // in-memory: nothing to flush

func (s *sliceStore) size() (int64, error) { return int64(len(s.buf)), nil }

func (s *sliceStore) setSize(n int64) error {
	if n < 0 {
		return ioError(errors.New("negative size"))
	}
	if n > int64(len(s.buf)) {
		s.buf = append(s.buf, make([]byte, n-int64(len(s.buf)))...)
	} else if n < int64(len(s.buf)) {
		s.buf = s.buf[:n]
	}
	return nil
}

func (s *sliceStore) close() error { return nil }

// storeOpKind tags the three mutating operations a commit issues at the blockStore seam.
type storeOpKind int

const (
	opWrite   storeOpKind = iota // a positioned page write (off, data)
	opSync                       // a durability barrier (no state change, but a crash boundary)
	opSetSize                    // a grow/truncate (reserve's preallocation)
)

// storeOp is one recorded blockStore operation. For opWrite, data is a private copy of the page
// bytes; for opSetSize, n is the target length. The ordered op log is what a crash replay interrupts.
type storeOp struct {
	kind storeOpKind
	off  int64
	data []byte
	n    int64
}

// recordingStore decorates a blockStore, appending every mutating call to ops before forwarding it.
// Driving a real commit through it captures the exact write/sync sequence persist emits (file.go),
// which applyCrash then replays up to an arbitrary power-loss point. Reads pass straight through and
// are not recorded (they do not change durable state, so they are irrelevant to crash recovery).
type recordingStore struct {
	base blockStore
	ops  []storeOp
}

func (r *recordingStore) readAt(off int64, length int) ([]byte, error) {
	return r.base.readAt(off, length)
}

func (r *recordingStore) writeAt(off int64, p []byte) error {
	d := make([]byte, len(p))
	copy(d, p)
	r.ops = append(r.ops, storeOp{kind: opWrite, off: off, data: d})
	return r.base.writeAt(off, p)
}

func (r *recordingStore) sync() error {
	r.ops = append(r.ops, storeOp{kind: opSync})
	return r.base.sync()
}

func (r *recordingStore) size() (int64, error) { return r.base.size() }

func (r *recordingStore) setSize(n int64) error {
	r.ops = append(r.ops, storeOp{kind: opSetSize, n: n})
	return r.base.setSize(n)
}

func (r *recordingStore) close() error { return r.base.close() }

// applyCrash reconstructs the on-disk image after a power loss that durably landed an arbitrary
// SUBSET of a commit's ops, modelling real device behavior soundly:
//
//   - Writes at or before the last completed sync in ops[0:cut] are barriered: always applied.
//   - Writes AFTER that last sync (in-flight, un-barriered) may each be independently lost — bit i
//     of dropMask drops the (i-th post-sync) write, modelling reordering / partial landing.
//   - The boundary op ops[cut], if a write, is TORN to its first tearBytes bytes (tearBytes < 0 ⇒
//     not applied at all); ops[cut+1:] are entirely lost.
//
// The barrier rule is what keeps the model sound: jed writes the meta page only AFTER the body sync,
// so the meta op is always post-sync while every body write it references is pre-sync (barriered) —
// applyCrash therefore can never fabricate the impossible "new meta published but a body page it
// points at went missing" state. Every image it produces is a real possible post-crash file.
func applyCrash(prior []byte, ops []storeOp, cut, tearBytes int, dropMask uint64) []byte {
	img := make([]byte, len(prior))
	copy(img, prior)
	put := func(off int64, data []byte) {
		end := off + int64(len(data))
		if end > int64(len(img)) {
			img = append(img, make([]byte, end-int64(len(img)))...)
		}
		copy(img[off:end], data)
	}
	apply := func(op storeOp) {
		switch op.kind {
		case opWrite:
			put(op.off, op.data)
		case opSetSize:
			if op.n > int64(len(img)) {
				img = append(img, make([]byte, op.n-int64(len(img)))...)
			} else if op.n < int64(len(img)) {
				img = img[:op.n]
			}
		case opSync:
			// barrier — no durable-state change
		}
	}
	if cut > len(ops) {
		cut = len(ops)
	}
	lastSync := -1
	for i := 0; i < cut; i++ {
		if ops[i].kind == opSync {
			lastSync = i
		}
	}
	postSyncWrite := 0
	for i := 0; i < cut; i++ {
		if ops[i].kind == opWrite && i > lastSync {
			drop := dropMask&(uint64(1)<<uint(postSyncWrite)) != 0
			postSyncWrite++
			if drop {
				continue // in-flight write lost
			}
		}
		apply(ops[i])
	}
	if tearBytes >= 0 && cut < len(ops) && ops[cut].kind == opWrite {
		data := ops[cut].data
		if tearBytes < len(data) {
			data = data[:tearBytes]
		}
		put(ops[cut].off, data)
	}
	return img
}

// openImage opens an in-memory database image through a fresh sliceStore — the demand-paged loader
// over a byte device, no file. Returns the loader's error (XX001/58030/…) unchanged on a malformed
// image.
func openImage(image []byte) (*engine, error) {
	store := newSliceStore(image)
	p, err := pagerFromStore(store)
	if err != nil {
		return nil, err
	}
	return loadEnginePaged(p, cacheLeaves(defaultCacheBytes, p.pageSize))
}

// errScanHang is returned by guardedScanIDs when an open+scan does not finish within the budget — a
// possible infinite loop, which the intrinsic oracle forbids. It is deliberately NOT an *EngineError,
// so callers reject it the same way they reject a panic.
var errScanHang = errors.New("open+scan did not return within the time budget (possible infinite loop)")

// guardedQuery opens image and runs sql, under a recover guard and a time budget. The intrinsic
// crash/corruption oracle reduces to inspecting the returned error:
//
//   - nil                 → the rows are usable (caller checks they match a valid snapshot).
//   - an *EngineError     → fail-closed (XX001 / 58030 / …) — acceptable.
//   - anything else        → a recovered panic ("panic: …") or errScanHang — a real bug; callers fail.
//
// A panic is converted to a plain error (not *EngineError); a hang trips errScanHang. The worker
// goroutine on a hang is left to leak — acceptable, the test is failing anyway.
func guardedQuery(image []byte, sql string) (rows [][]Value, err error) {
	type result struct {
		rows [][]Value
		err  error
	}
	ch := make(chan result, 1)
	go func() {
		defer func() {
			if r := recover(); r != nil {
				ch <- result{nil, fmt.Errorf("panic: %v", r)}
			}
		}()
		db, e := openImage(image)
		if e != nil {
			ch <- result{nil, e}
			return
		}
		defer db.Close()
		out, e := execute(db, sql)
		if e != nil {
			ch <- result{nil, e}
			return
		}
		ch <- result{out.Rows, nil}
	}()
	select {
	case res := <-ch:
		return res.rows, res.err
	case <-time.After(10 * time.Second):
		return nil, errScanHang
	}
}

// guardedScanIDs runs `SELECT id FROM t ORDER BY id` through guardedQuery, returning the id column.
func guardedScanIDs(image []byte) ([]int64, error) {
	rows, err := guardedQuery(image, "SELECT id FROM t ORDER BY id")
	if err != nil {
		return nil, err
	}
	ids := make([]int64, len(rows))
	for i, row := range rows {
		ids[i] = row[0].Int
	}
	return ids, nil
}

// renderRows renders a result set to plain strings — a stable, comparable snapshot of the rows
// (every cell via Value.Render, the conformance renderer) for the corruption oracle's set membership.
func renderRows(rows [][]Value) [][]string {
	out := make([][]string, len(rows))
	for i, row := range rows {
		cells := make([]string, len(row))
		for j, v := range row {
			cells[j] = v.Render()
		}
		out[i] = cells
	}
	return out
}

// (Rendered-row set equality reuses equalRows from checksum_test.go.)

// isEngineError reports whether err is a structured engine error (a fail-closed SQLSTATE) — the
// acceptable failure shape. A recovered panic or errScanHang is not one, so it fails the oracle.
// (Row-set equality reuses equalIDs from crash_recovery_test.go.)
func isEngineError(err error) bool {
	var ee *EngineError
	return errors.As(err, &ee)
}
