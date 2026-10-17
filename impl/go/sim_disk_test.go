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
	"io"
	"sort"
	"testing"
)

// Decision lanes keep independent choices from correlating (§6.2). The values are part of the
// cross-core contract: every core's runner uses the same lane for the same decision.
const (
	simLaneSurvive = 1  // power loss: does anything land at this offset?
	simLaneVersion = 2  // power loss: which pending version lands
	simLaneTear    = 3  // power loss: is the landing version torn?
	simLaneSector  = 4  // power loss: per-sector old/new choice of a torn version
	simLaneIOWrite = 5  // io_error op=write: does this write fail?
	simLaneIssue   = 6  // process_crash: was this write issued before the process died?
	simLaneLand    = 7  // io_error op=sync: does this pending write land or vanish (fsyncgate)?
	simLaneSession = 8  // a session's entropy-seam seed
	simLaneRead    = 9  // read_error: do reads of this offset fail during the step?
	simLaneDir     = 10 // power loss: how many pending directory changes survive (% changes+1)
	simLaneSpill   = 11 // spill_error: does this spill-file operation fail?
	simLanePage    = 12 // corrupt: which page a flip lands in
	simLaneBit     = 13 // corrupt: which bit of that page it flips
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
	simReadError  // per-core: which reads happen depends on buffer-pool policy
	simENOSPC     // per-core: when the file grows depends on preallocation policy
	simSpillError // per-core: what a core spills, and in which operations, is its own codec's business
	simDisconnect // a page-service connection ends at the step's nth frame (S8); the network's, not the disk's
)

// simFault is one armed fault (simulation-testing.md §6.3). Permille fields are 0..1000.
type simFault struct {
	kind    simFaultKind
	survive int // power_loss
	tear    int // power_loss
	issue   int // process_crash
	p       int // io_error op=write, read_error, spill_error
	nth     int // power_loss/process_crash: the barrier it fires at (default 1); io_error op=sync: the sync
}

// simMainFile is the database file's name in the simulated filesystem — the path the engine is given.
const simMainFile = simDiskPath

// simFS is the simulated filesystem (§6.1, slice S4): named files, each a simDisk, and a directory
// whose entries become durable only at a directory barrier. It outlives every Database opened over
// it: a crash discards the process but keeps the disk, as a real filesystem would. The per-step
// fault state lives here, because a fault arms a step, not a file.
type simFS struct {
	seed     uint64
	sector   int
	pageSize int64

	files   map[string]*simDisk // the kernel's view of the directory
	durable map[string]*simDisk // the directory as of the last directory barrier
	dirOps  []simDirOp          // directory changes since that barrier, in order

	// Per-step state, reset by beginStep.
	step     uint64
	fault    *simFault
	fired    string // "" | "barrier" | "write" | "sync" | "read" | "enospc"
	dead     bool   // the process died mid-step; every later call fails
	syncs    int    // file syncs this step (io_error op=sync nth)
	barriers int    // file and directory barriers this step (power_loss/process_crash nth)
	writes   int
	oracle   bool   // the runner's own oracle reads are in progress: read faults do not apply
	spillOps uint64 // spill-file operations this step (spill_error decisions)

	ops uint64 // FNV-1a fold of every mutating call, for the self-determinism check (§8.5)
}

// simDirOp is one directory change: create a file, rename one over another, or remove one.
type simDirOp struct {
	kind     byte // 'c' create, 'r' rename, 'd' remove
	from, to string
	file     *simDisk
}

// simDisk is one simulated file: durable bytes that survive a power loss unconditionally; pending,
// per byte offset, every version written since that offset was last covered by a barrier; and
// cache, what reads see (durable overlaid with the newest pending version).
type simDisk struct {
	fs      *simFS
	key     uint64 // folded into fault decisions' offsets; 0 for the database file
	durable []byte
	cache   []byte
	pending map[int64][][]byte

	// Per-step state, reset by beginStep: whether this file's own barrier completed with every
	// directory change durable (the floor, §8.2), how many writes reached it, and which pages.
	barrierOK bool
	writes    int
	pages     map[int64]bool

	// unissued are the writes an armed process_crash decided never reached the kernel; processDeath
	// removes them.
	unissued []simWrite
}

type simWrite struct {
	off     int64
	version int
}

// simFileKey keeps two files' fault decisions independent (§6.2): the database file's is 0, so its
// decisions are unchanged by the existence of other files; another file's is its name's FNV-1a 32
// plus one, shifted above any offset.
func simFileKey(name string) uint64 {
	if name == simMainFile {
		return 0
	}
	h := fnv.New32a()
	h.Write([]byte(name))
	return (uint64(h.Sum32()) + 1) << 40
}

// newSimFS is an empty simulated filesystem; the runner creates the database in it (S4).
func newSimFS(seed uint64, sector int, pageSize uint32) *simFS {
	return &simFS{
		seed: seed, sector: sector, pageSize: int64(pageSize), files: map[string]*simDisk{},
		durable: map[string]*simDisk{}, ops: 14695981039346656037,
	}
}

func (fs *simFS) newFile(name string) *simDisk {
	return &simDisk{fs: fs, key: simFileKey(name), pending: make(map[int64][][]byte), pages: map[int64]bool{}}
}

// main is the database file, or nil if a crash lost it.
func (fs *simFS) main() *simDisk { return fs.files[simMainFile] }

func simInjected(what string) error {
	return newError(IoError, "I/O error: simulated "+what)
}

// beginStep resets the per-step state and arms f (nil for an unfaulted step).
func (fs *simFS) beginStep(step uint64, f *simFault) {
	fs.step = step
	fs.fault = f
	fs.fired = ""
	fs.dead = false
	fs.syncs = 0
	fs.barriers = 0
	fs.writes = 0
	fs.spillOps = 0
	for _, d := range fs.allFiles() {
		d.unissued = nil
		d.barrierOK = false
		d.writes = 0
		d.pages = map[int64]bool{}
	}
}

// allFiles lists every file the directory, its durable state, or a pending change refers to, each
// once, in a deterministic order.
func (fs *simFS) allFiles() []*simDisk {
	seen := map[*simDisk]bool{}
	var out []*simDisk
	add := func(d *simDisk) {
		if d != nil && !seen[d] {
			seen[d] = true
			out = append(out, d)
		}
	}
	for _, dir := range []map[string]*simDisk{fs.files, fs.durable} {
		names := make([]string, 0, len(dir))
		for name := range dir {
			names = append(names, name)
		}
		sort.Strings(names)
		for _, name := range names {
			add(dir[name])
		}
	}
	for _, op := range fs.dirOps {
		add(op.file)
	}
	return out
}

func (fs *simFS) fold(kind byte, key uint64, off int64, p []byte) {
	h := fnv.New64a()
	var buf [25]byte
	binary.BigEndian.PutUint64(buf[0:8], fs.ops)
	buf[8] = kind
	binary.BigEndian.PutUint64(buf[9:17], key)
	binary.BigEndian.PutUint64(buf[17:25], uint64(off))
	h.Write(buf[:])
	h.Write(p)
	fs.ops = h.Sum64()
}

// barrier counts one file or directory barrier and reports whether an armed power loss or process
// crash fires at it: the barrier never completes, because the machine or process is gone.
func (fs *simFS) barrier() bool {
	fs.barriers++
	f := fs.fault
	if f == nil || fs.fired != "" || (f.kind != simPowerLoss && f.kind != simProcessCrash) {
		return false
	}
	nth := f.nth
	if nth <= 0 {
		nth = 1
	}
	if fs.barriers != nth {
		return false
	}
	fs.fired = "barrier"
	fs.dead = true
	return true
}

func (d *simDisk) decide(lane, step, off, version uint64) uint64 {
	return simDecide(d.fs.seed, lane, step, off+d.key, version)
}

func (d *simDisk) readAt(off int64, length int) ([]byte, error) {
	fs := d.fs
	if f := fs.fault; f != nil && f.kind == simReadError && !fs.dead && !fs.oracle &&
		d.decide(simLaneRead, fs.step, uint64(off), 0)%1000 < uint64(f.p) {
		fs.fired = "read"
		return nil, simInjected("read error")
	}
	if off < 0 || off+int64(length) > int64(len(d.cache)) {
		return nil, newError(IoError, "I/O error: short read past the end of the simulated disk")
	}
	return append([]byte(nil), d.cache[off:off+int64(length)]...), nil
}

func (d *simDisk) writeAt(off int64, p []byte) error {
	fs := d.fs
	fs.fold('w', d.key, off, p)
	if fs.dead {
		return simInjected("write after process death")
	}
	version := uint64(len(d.pending[off]))
	if f := fs.fault; f != nil {
		switch f.kind {
		case simProcessCrash:
			// The live process sees all its own writes; only at its death does a write that was never
			// issued to the kernel disappear (processDeath).
			if d.decide(simLaneIssue, fs.step, uint64(off), version)%1000 >= uint64(f.issue) {
				d.unissued = append(d.unissued, simWrite{off: off, version: int(version)})
			}
		case simIOWrite:
			if fs.fired == "" && d.decide(simLaneIOWrite, fs.step, uint64(off), version)%1000 < uint64(f.p) {
				fs.fired = "write"
				return simInjected("write error")
			}
		}
	}
	fs.writes++
	d.writes++
	for pg := off / fs.pageSize; pg*fs.pageSize < off+int64(len(p)); pg++ {
		d.pages[pg] = true
	}
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
	// Only a database file's own barrier, with its directory entry durable, makes the generation it
	// holds durable (the floor, §8.2); a compaction's temp file syncing proves nothing about it.
	if len(d.fs.dirOps) == 0 {
		d.barrierOK = true
	}
	d.unissued = nil // reaching a barrier means every earlier write was issued
}

func (d *simDisk) sync() error {
	fs := d.fs
	fs.fold('s', d.key, 0, nil)
	if fs.dead {
		return simInjected("sync after process death")
	}
	fs.syncs++
	if fs.barrier() {
		return simInjected("crash at a durability barrier")
	}
	if f := fs.fault; f != nil && f.kind == simIOSync && fs.fired == "" && fs.syncs == f.nth {
		fs.fired = "sync"
		d.fsyncgate()
		return simInjected("sync error")
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
		if d.decide(simLaneLand, d.fs.step, uint64(off), uint64(len(vs)-1))%2 == 0 {
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
// hosts.md §2.1) or truncates. It is never a fault point for a crash (simulation-testing.md §6.3).
func (d *simDisk) setSize(n int64) error {
	fs := d.fs
	fs.fold('z', d.key, n, nil)
	if fs.dead {
		return simInjected("set_size after process death")
	}
	cur := int64(len(d.cache))
	if f := fs.fault; f != nil && f.kind == simENOSPC && n > cur && fs.fired == "" {
		fs.fired = "enospc"
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

// powerLoss replaces one file's contents with what survives a power cut during step (§6.3): for
// each pending offset, nothing lands or one pending version (any, not only the newest) lands,
// possibly torn sector by sector against the durable bytes. The cache is discarded.
func (d *simDisk) powerLoss(step uint64, survive, tear int) {
	sector := d.fs.sector
	for _, off := range d.sortedPending() {
		vs := d.pending[off]
		if d.decide(simLaneSurvive, step, uint64(off), 0)%1000 >= uint64(survive) {
			continue
		}
		idx := d.decide(simLaneVersion, step, uint64(off), 0) % uint64(len(vs))
		data := append([]byte(nil), vs[idx]...)
		if d.decide(simLaneTear, step, uint64(off), idx)%1000 < uint64(tear) {
			for s := 0; s < len(data); s += sector {
				if d.decide(simLaneSector, step, uint64(off)+uint64(s), idx)&1 == 1 {
					continue // this sector took the new bytes
				}
				for i := s; i < s+sector && i < len(data); i++ {
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

// powerLoss applies a power cut to the whole filesystem: every file loses what its own decisions
// say, and the directory keeps its durable entries plus an ordered prefix of the pending changes —
// a journaling filesystem applies metadata changes in order (§6.3).
func (fs *simFS) powerLoss(step uint64, survive, tear int) {
	for _, d := range fs.allFiles() {
		d.powerLoss(step, survive, tear)
	}
	keep := int(simDecide(fs.seed, simLaneDir, step, 0, 0) % uint64(len(fs.dirOps)+1))
	dir := make(map[string]*simDisk, len(fs.durable))
	for name, d := range fs.durable {
		dir[name] = d
	}
	for _, op := range fs.dirOps[:keep] {
		applyDirOp(dir, op)
	}
	fs.files = dir
	fs.durable = make(map[string]*simDisk, len(dir))
	for name, d := range dir {
		fs.durable[name] = d
	}
	fs.dirOps = nil
}

func applyDirOp(dir map[string]*simDisk, op simDirOp) {
	switch op.kind {
	case 'c':
		dir[op.to] = op.file
	case 'r':
		dir[op.to] = op.file
		delete(dir, op.from)
	case 'd':
		delete(dir, op.from)
	}
}

// processDeath ends the process: the kernel cache and directory survive, every issued write still
// pending (not durable), so a later power loss can still drop it (§6.3).
func (fs *simFS) processDeath() {
	for _, d := range fs.allFiles() {
		d.processDeath()
	}
	fs.fault = nil
	fs.dead = false
}

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
}

// --- the fileHost the simulated database's compaction goes through (filehost.go) ---------------

func (fs *simFS) realPath(path string) (string, error) { return path, nil }

func (fs *simFS) exists(path string) (bool, error) {
	if fs.dead {
		return false, simInjected("stat after process death")
	}
	return fs.files[path] != nil, nil
}

func (fs *simFS) writeFile(path, _ string, noSync bool, fill func(writeAt func(off int64, p []byte) error) error) error {
	if fs.dead {
		return simInjected("create after process death")
	}
	d := fs.newFile(path)
	fs.files[path] = d
	fs.dirOps = append(fs.dirOps, simDirOp{kind: 'c', to: path, file: d})
	fs.fold('c', d.key, 0, nil)
	if err := fill(d.writeAt); err != nil {
		return err
	}
	if !noSync {
		return d.sync()
	}
	return nil
}

func (fs *simFS) rename(from, to string) error {
	if fs.dead {
		return simInjected("rename after process death")
	}
	d := fs.files[from]
	if d == nil {
		return newError(IoError, "I/O error: rename of a missing file "+from)
	}
	fs.files[to] = d
	delete(fs.files, from)
	fs.dirOps = append(fs.dirOps, simDirOp{kind: 'r', from: from, to: to, file: d})
	fs.fold('r', d.key, 0, nil)
	return nil
}

func (fs *simFS) remove(path string) {
	if fs.dead {
		return
	}
	if d := fs.files[path]; d != nil {
		delete(fs.files, path)
		fs.dirOps = append(fs.dirOps, simDirOp{kind: 'd', from: path, file: d})
		fs.fold('d', d.key, 0, nil)
	}
}

// syncDir is a fault point only when it has something to make durable: a directory barrier with no
// pending change cannot lose anything, and counting it would shift every later barrier's number.
func (fs *simFS) syncDir(string) {
	fs.fold('D', 0, 0, nil)
	if fs.dead || len(fs.dirOps) == 0 || fs.barrier() {
		return
	}
	fs.durable = make(map[string]*simDisk, len(fs.files))
	for name, d := range fs.files {
		fs.durable[name] = d
	}
	fs.dirOps = nil
}

func (fs *simFS) openStore(path string, _, _ bool) (blockStore, error) {
	if fs.dead {
		return nil, simInjected("open after process death")
	}
	d := fs.files[path]
	if d == nil {
		return nil, newError(UndefinedFile, "database file does not exist: "+path)
	}
	return d, nil
}

// --- the spill host (spillhost.go): scratch files in memory, whose operations can fail ----------

// spillHost is the simulated spill host. Scratch bytes are never durable state, so its files live
// outside the filesystem and vanish with the process; spill_error fails an operation (S3).
func (fs *simFS) spillHost() spillHost { return simSpill{fs: fs} }

type simSpill struct{ fs *simFS }

type simSpillFile struct {
	fs     *simFS
	data   []byte
	closed bool
}

// spillFault reports whether the step's armed spill_error fails the next spill operation: each
// operation is decided by its ordinal in the step (lane 11). Which operations a core performs is
// its own business, so the fault is per-core (§6.3).
func (fs *simFS) spillFault() error {
	if fs.dead {
		return simInjected("spill after process death")
	}
	n := fs.spillOps
	fs.spillOps++
	if f := fs.fault; f != nil && f.kind == simSpillError && !fs.oracle &&
		simDecide(fs.seed, simLaneSpill, fs.step, n, 0)%1000 < uint64(f.p) {
		fs.fired = "spill"
		return simInjected("spill I/O error")
	}
	return nil
}

func (h simSpill) create(string) (spillFile, error) {
	if err := h.fs.spillFault(); err != nil {
		return nil, err
	}
	return &simSpillFile{fs: h.fs}, nil
}

func (f *simSpillFile) ReadAt(p []byte, off int64) (int, error) {
	if err := f.fs.spillFault(); err != nil {
		return 0, err
	}
	if off >= int64(len(f.data)) {
		return 0, io.EOF
	}
	n := copy(p, f.data[off:])
	if n < len(p) {
		return n, io.EOF
	}
	return n, nil
}

func (f *simSpillFile) WriteAt(p []byte, off int64) (int, error) {
	if err := f.fs.spillFault(); err != nil {
		return 0, err
	}
	if end := off + int64(len(p)); end > int64(len(f.data)) {
		f.data = append(f.data, make([]byte, end-int64(len(f.data)))...)
	}
	return copy(f.data[off:], p), nil
}

func (f *simSpillFile) size() (int64, error) { return int64(len(f.data)), nil }

func (f *simSpillFile) truncate(n int64) error {
	if n <= int64(len(f.data)) {
		f.data = f.data[:n]
	} else {
		f.data = append(f.data, make([]byte, n-int64(len(f.data)))...)
	}
	return nil
}

func (f *simSpillFile) close() error {
	f.closed, f.data = true, nil
	return nil
}

// --- corruption (simulation-testing.md §6.3 `corrupt`) -------------------------------------------

// simFlip is one flipped bit, remembered so the step can repair it.
type simFlip struct {
	page int64
	orig []byte // the page's durable bytes before the flip
}

// corrupt flips n bits in d's durable body pages below pageCount, choosing each page (lane 12) and
// bit (lane 13) by the flip's ordinal, and skipping a page with a pending write: bit rot strikes data
// at rest. Both durable and cache change, as if the kernel's cached copy were dropped too. With
// reseal, each touched page's CRC is recomputed, so only the engine's structural checks stand
// between the flip and a wrong answer. Returns what to restore.
func (d *simDisk) corrupt(step uint64, n int, pageCount uint32, reseal bool) []simFlip {
	ps := d.fs.pageSize
	if pageCount <= 2 {
		return nil
	}
	var flips []simFlip
	touched := map[int64]bool{}
	for k := 0; k < n; k++ {
		pg := 2 + int64(simDecide(d.fs.seed, simLanePage, step, uint64(k), 0)%uint64(pageCount-2))
		bit := int64(simDecide(d.fs.seed, simLaneBit, step, uint64(k), 0) % uint64(ps*8))
		start := pg * ps
		if start+ps > int64(len(d.durable)) || start+ps > int64(len(d.cache)) || d.pendingIn(start, start+ps) {
			continue
		}
		if !touched[pg] {
			touched[pg] = true
			flips = append(flips, simFlip{page: pg, orig: append([]byte(nil), d.durable[start:start+ps]...)})
		}
		d.durable[start+bit/8] ^= 1 << (bit % 8)
	}
	for _, f := range flips {
		start := f.page * ps
		page := d.durable[start : start+ps]
		if reseal {
			binary.BigEndian.PutUint32(page[12:16], pageCRC(page))
		}
		copy(d.cache[start:start+ps], page)
	}
	return flips
}

// repair restores the pages corrupt flipped.
func (d *simDisk) repair(flips []simFlip) {
	for _, f := range flips {
		start := f.page * d.fs.pageSize
		copy(d.durable[start:], f.orig)
		copy(d.cache[start:], f.orig)
	}
}

// pendingIn reports whether any pending write overlaps [lo, hi).
func (d *simDisk) pendingIn(lo, hi int64) bool {
	for off, vs := range d.pending {
		if off < hi && off+int64(len(vs[len(vs)-1])) > lo {
			return true
		}
	}
	return false
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
