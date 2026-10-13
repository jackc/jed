package jed

// The deterministic-simulation disk (spec/design/simulation-testing.md §6): a blockStore whose
// durability, crashes, and I/O errors are decided by a seeded, content-keyed function, so a whole
// multi-commit history replays exactly from its seed and a crash image is identical in every core.
//
// State (§6.1): durable bytes survive a power loss unconditionally; pending holds, per byte offset,
// every version written since that offset was last covered by a barrier; cache is what reads see
// (durable overlaid with the newest pending version). sync and a growing setSize are barriers.
//
// Every fault decision is simDecide(seed, lane, step, offset, version) (§6.2): keyed by WHAT was
// written WHERE, never by a write's position in the op log, so cores whose write order differs
// within a commit still produce the same post-crash bytes.

import (
	"encoding/binary"
	"hash/fnv"
	"sort"
	"testing"
)

// Decision lanes keep independent choices from correlating (§6.2). The values are part of the
// cross-core contract: every core's runner uses the same lane for the same decision.
const (
	simLaneSurvive = 1 // power loss: does anything land at this offset?
	simLaneVersion = 2 // power loss: which pending version lands
	simLaneTear    = 3 // power loss: is the landing version torn?
	simLaneSector  = 4 // power loss: per-sector old/new choice of a torn version
	simLaneIOWrite = 5 // io_error op=write: does this write fail?
	simLaneIssue   = 6 // process_crash: was this write issued before the process died?
	simLaneLand    = 7 // io_error op=sync: does this pending write land or vanish (fsyncgate)?
	simLaneSession = 8 // a session's entropy-seam seed
	simLaneRead    = 9 // read_error: do reads of this offset fail during the step?
)

const simGamma = 0x9E3779B97F4A7C15

// simMix is the splitmix64 finalizer (spec/encoding/prng.toml's mixer).
func simMix(z uint64) uint64 {
	z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9
	z = (z ^ (z >> 27)) * 0x94D049BB133111EB
	return z ^ (z >> 31)
}

// simDecide is the content-keyed decision function (simulation-testing.md §6.2).
func simDecide(seed, lane, step, offset, version uint64) uint64 {
	h := seed
	for _, w := range [4]uint64{lane, step, offset, version} {
		h = simMix((h ^ w) + simGamma)
	}
	return h
}

type simFaultKind int

const (
	simPowerLoss simFaultKind = iota + 1
	simProcessCrash
	simIOWrite
	simIOSync
	simReadError // per-core: which reads happen depends on buffer-pool policy
	simENOSPC    // per-core: when the file grows depends on preallocation policy
)

// simFault is one armed fault (simulation-testing.md §6.3). Permille fields are 0..1000.
type simFault struct {
	kind    simFaultKind
	survive int // power_loss
	tear    int // power_loss
	issue   int // process_crash
	p       int // io_error op=write, read_error
	nth     int // io_error op=sync
}

// simDisk is the simulated device. It outlives every Database opened over it: a crash discards the
// process (the Database) but keeps the disk, as a real file would.
type simDisk struct {
	seed    uint64
	sector  int
	durable []byte
	cache   []byte
	pending map[int64][][]byte

	// Per-step state, reset by beginStep.
	step      uint64
	fault     *simFault
	fired     string // "" | "barrier" | "write" | "sync"
	dead      bool   // the process died mid-step; every later call fails
	syncs     int
	writes    int
	barrierOK bool // a barrier completed this step
	oracle    bool // the runner's own oracle reads are in progress: read faults do not apply

	ops uint64 // FNV-1a fold of every mutating call, for the self-determinism check (§8.5)

	// unissued are the writes an armed process_crash decided never reached the kernel; processDeath
	// removes them.
	unissued []simWrite
}

type simWrite struct {
	off     int64
	version int
}

func newSimDisk(seed uint64, sector int, image []byte) *simDisk {
	return &simDisk{
		seed:    seed,
		sector:  sector,
		durable: append([]byte(nil), image...),
		cache:   append([]byte(nil), image...),
		pending: make(map[int64][][]byte),
		ops:     14695981039346656037,
	}
}

func simInjected(what string) error {
	return newError(IoError, "I/O error: simulated "+what)
}

// beginStep resets the per-step state and arms f (nil for an unfaulted step).
func (d *simDisk) beginStep(step uint64, f *simFault) {
	d.step = step
	d.fault = f
	d.fired = ""
	d.dead = false
	d.syncs = 0
	d.writes = 0
	d.barrierOK = false
	d.unissued = nil
}

func (d *simDisk) fold(kind byte, off int64, p []byte) {
	h := fnv.New64a()
	var buf [17]byte
	binary.BigEndian.PutUint64(buf[0:8], d.ops)
	buf[8] = kind
	binary.BigEndian.PutUint64(buf[9:17], uint64(off))
	h.Write(buf[:])
	h.Write(p)
	d.ops = h.Sum64()
}

func (d *simDisk) readAt(off int64, length int) ([]byte, error) {
	if f := d.fault; f != nil && f.kind == simReadError && !d.dead && !d.oracle &&
		simDecide(d.seed, simLaneRead, d.step, uint64(off), 0)%1000 < uint64(f.p) {
		d.fired = "read"
		return nil, simInjected("read error")
	}
	if off < 0 || off+int64(length) > int64(len(d.cache)) {
		return nil, newError(IoError, "I/O error: short read past the end of the simulated disk")
	}
	return append([]byte(nil), d.cache[off:off+int64(length)]...), nil
}

func (d *simDisk) writeAt(off int64, p []byte) error {
	d.fold('w', off, p)
	if d.dead {
		return simInjected("write after process death")
	}
	version := uint64(len(d.pending[off]))
	if f := d.fault; f != nil {
		switch f.kind {
		case simProcessCrash:
			// The live process sees all its own writes; only at its death does a write that was never
			// issued to the kernel disappear (processDeath).
			if simDecide(d.seed, simLaneIssue, d.step, uint64(off), version)%1000 >= uint64(f.issue) {
				d.unissued = append(d.unissued, simWrite{off: off, version: int(version)})
			}
		case simIOWrite:
			if d.fired == "" && simDecide(d.seed, simLaneIOWrite, d.step, uint64(off), version)%1000 < uint64(f.p) {
				d.fired = "write"
				return simInjected("write error")
			}
		}
	}
	d.writes++
	end := off + int64(len(p))
	if end > int64(len(d.cache)) {
		d.cache = append(d.cache, make([]byte, end-int64(len(d.cache)))...)
	}
	copy(d.cache[off:end], p)
	d.pending[off] = append(d.pending[off], append([]byte(nil), p...))
	return nil
}

// sortedPending returns the pending offsets in ascending order: no map order may leak.
func (d *simDisk) sortedPending() []int64 {
	offs := make([]int64, 0, len(d.pending))
	for off := range d.pending {
		offs = append(offs, off)
	}
	sort.Slice(offs, func(a, b int) bool { return offs[a] < offs[b] })
	return offs
}

func (d *simDisk) putDurable(off int64, p []byte) {
	end := off + int64(len(p))
	if end > int64(len(d.durable)) {
		d.durable = append(d.durable, make([]byte, end-int64(len(d.durable)))...)
	}
	copy(d.durable[off:end], p)
}

// flush makes every pending write durable: a successful barrier.
func (d *simDisk) flush() {
	for _, off := range d.sortedPending() {
		vs := d.pending[off]
		d.putDurable(off, vs[len(vs)-1])
	}
	if len(d.durable) < len(d.cache) {
		d.durable = append(d.durable, make([]byte, len(d.cache)-len(d.durable))...)
	}
	d.pending = make(map[int64][][]byte)
	d.barrierOK = true
	d.unissued = nil // reaching a barrier means every earlier write was issued
}

func (d *simDisk) sync() error {
	d.fold('s', 0, nil)
	if d.dead {
		return simInjected("sync after process death")
	}
	d.syncs++
	if f := d.fault; f != nil && d.fired == "" {
		switch f.kind {
		case simPowerLoss, simProcessCrash:
			// The barrier never completes: the machine or process is gone.
			d.fired = "barrier"
			d.dead = true
			return simInjected("crash at a durability barrier")
		case simIOSync:
			if d.syncs == f.nth {
				d.fired = "sync"
				d.fsyncgate()
				return simInjected("sync error")
			}
		}
	}
	d.flush()
	return nil
}

// fsyncgate models a failed fsync on Linux: each pending write independently became durable or was
// dropped from the cache, and the error is reported once (§6.3).
func (d *simDisk) fsyncgate() {
	for _, off := range d.sortedPending() {
		vs := d.pending[off]
		newest := vs[len(vs)-1]
		if simDecide(d.seed, simLaneLand, d.step, uint64(off), uint64(len(vs)-1))%2 == 0 {
			d.putDurable(off, newest)
			continue
		}
		end := off + int64(len(newest))
		for i := off; i < end; i++ {
			if i < int64(len(d.durable)) {
				d.cache[i] = d.durable[i]
			} else {
				d.cache[i] = 0
			}
		}
	}
	d.pending = make(map[int64][][]byte)
}

func (d *simDisk) size() (int64, error) { return int64(len(d.cache)), nil }

// setSize grows durably (zero blocks + a full fsync, which also flushes every pending write —
// hosts.md §2.1) or truncates. It is never a fault point (simulation-testing.md §6.3).
func (d *simDisk) setSize(n int64) error {
	d.fold('z', n, nil)
	if d.dead {
		return simInjected("set_size after process death")
	}
	cur := int64(len(d.cache))
	if f := d.fault; f != nil && f.kind == simENOSPC && n > cur && d.fired == "" {
		d.fired = "enospc"
		return simInjected("no space left on device")
	}
	switch {
	case n > cur:
		d.cache = append(d.cache, make([]byte, n-cur)...)
		d.flush()
	case n < cur:
		d.cache = d.cache[:n]
		if int64(len(d.durable)) > n {
			d.durable = d.durable[:n]
		}
		for off := range d.pending {
			if off >= n {
				delete(d.pending, off)
			}
		}
	}
	return nil
}

func (d *simDisk) close() error { return nil }

// powerLoss replaces the device contents with what survives a power cut during step (§6.3): for
// each pending offset, nothing lands or one pending version (any, not only the newest) lands,
// possibly torn sector by sector against the durable bytes. The cache is discarded.
func (d *simDisk) powerLoss(step uint64, survive, tear int) {
	for _, off := range d.sortedPending() {
		vs := d.pending[off]
		if simDecide(d.seed, simLaneSurvive, step, uint64(off), 0)%1000 >= uint64(survive) {
			continue
		}
		idx := simDecide(d.seed, simLaneVersion, step, uint64(off), 0) % uint64(len(vs))
		data := append([]byte(nil), vs[idx]...)
		if simDecide(d.seed, simLaneTear, step, uint64(off), idx)%1000 < uint64(tear) {
			for s := 0; s < len(data); s += d.sector {
				if simDecide(d.seed, simLaneSector, step, uint64(off)+uint64(s), idx)&1 == 1 {
					continue // this sector took the new bytes
				}
				for i := s; i < s+d.sector && i < len(data); i++ {
					at := off + int64(i)
					if at < int64(len(d.durable)) {
						data[i] = d.durable[at]
					} else {
						data[i] = 0
					}
				}
			}
		}
		d.putDurable(off, data)
	}
	d.pending = make(map[int64][][]byte)
	d.cache = append([]byte(nil), d.durable...)
}

// processDeath ends the process: the kernel cache survives with every issued write still pending
// (not durable), so a later power loss can still drop it (§6.3).
func (d *simDisk) processDeath() {
	// Undo the never-issued writes, newest first so each version index stays valid. A barrier that
	// completed after such a write already made it durable — the process could not have reached
	// that barrier without issuing it — so only still-pending versions are removed.
	for i := len(d.unissued) - 1; i >= 0; i-- {
		w := d.unissued[i]
		vs := d.pending[w.off]
		if w.version >= len(vs) {
			continue
		}
		size := len(vs[w.version])
		vs = append(vs[:w.version:w.version], vs[w.version+1:]...)
		if len(vs) == 0 {
			delete(d.pending, w.off)
		} else {
			d.pending[w.off] = vs
		}
		for j := 0; j < size; j++ {
			at := w.off + int64(j)
			switch {
			case len(vs) > 0:
				d.cache[at] = vs[len(vs)-1][j]
			case at < int64(len(d.durable)):
				d.cache[at] = d.durable[at]
			default:
				d.cache[at] = 0
			}
		}
	}
	d.unissued = nil
	d.fault = nil
	d.dead = false
}

// TestSimDecideVectors pins simDecide to the shared vectors in spec/encoding/prng.toml
// ([[sim_decide]]), which spec/encoding/prng_verify.rb recomputes independently.
func TestSimDecideVectors(t *testing.T) {
	for _, v := range []struct {
		seed, lane, step, offset, version, want uint64
	}{
		{0, 1, 0, 0, 0, 0xe28195ddd9ee4956},
		{1, 1, 7, 8192, 0, 0xa26f2f0c00568058},
		{81723, 2, 42, 16384, 3, 0x1ec7ed20c6738508},
		{9223372036854775807, 4, 300, 640, 1, 0x925c4929a8b5d11b},
		{12345, 8, 9, 0, 0, 0x47a4d3845325ea26},
	} {
		if got := simDecide(v.seed, v.lane, v.step, v.offset, v.version); got != v.want {
			t.Errorf("simDecide(%d, %d, %d, %d, %d) = %016x, want %016x", v.seed, v.lane, v.step, v.offset, v.version, got, v.want)
		}
	}
}
