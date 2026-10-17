package jed

// The deterministic-simulation lock host (spec/design/simulation-testing.md §14 S5): simulated
// processes sharing one table of whole-file locks, standing in for the OS host of
// coordinator_host.go. A process that dies loses its locks at once, as the kernel releases a dead
// process's locks. A contended acquisition never blocks the single simulation thread: each poll runs
// the other processes' presence probes and advances the simulated clock, so the holder can react and
// deadlines expire in simulated time.

import (
	"fmt"
	"sort"
	"sync/atomic"
	"time"
)

const (
	simLockPoll     = 2000   // microseconds of simulated time per poll of a contended lock
	simLockMaxPolls = 100000 // polls without success before a wait is reported as a deadlock
)

// simLockTable is the kernel's view of every bundle lock: lock name → holding process → exclusive.
type simLockTable struct {
	holders map[string]map[int]bool
	clock   *int64             // the runner's simulated clock, in microseconds
	probes  map[int]func()     // each live process's presence probe
	busy    func(pid int) bool // whether a process's probe would block (it holds its writer gate)
	polls   int
}

func newSimLockTable(clock *int64, busy func(int) bool) *simLockTable {
	return &simLockTable{holders: map[string]map[int]bool{}, clock: clock, probes: map[int]func(){}, busy: busy}
}

// release drops every lock pid holds and forgets its probe: the process is gone.
func (t *simLockTable) release(pid int) {
	for _, h := range t.holders {
		delete(h, pid)
	}
	delete(t.probes, pid)
}

// runProbes runs every live process's probe except skip's, in process order.
func (t *simLockTable) runProbes(skip int) {
	pids := make([]int, 0, len(t.probes))
	for pid := range t.probes {
		pids = append(pids, pid)
	}
	sort.Ints(pids)
	for _, pid := range pids {
		if probe := t.probes[pid]; pid != skip && probe != nil && !t.busy(pid) {
			probe()
		}
	}
}

// simLockHost is one simulated process's lockHost.
type simLockHost struct {
	table   *simLockTable
	pidNum  int
	claimed map[string]bool // the process-local open registry
	net     *simNet         // the page service's records, endpoints, and connections (sim_net_test.go)
}

func (h *simLockHost) claimPath(path string) bool {
	if h.claimed[path] {
		return false
	}
	h.claimed[path] = true
	return true
}

func (h *simLockHost) releasePath(path string) { delete(h.claimed, path) }

func (h *simLockHost) openBundle(string) (lockFiles, error) {
	return lockFiles{presence: "presence", arrival: "arrival"}, nil
}

func (h *simLockHost) validateMarker(string) error { return nil }

func (h *simLockHost) closeFiles(lockFiles) {
	for _, holders := range h.table.holders {
		delete(holders, h.pidNum)
	}
}

// tryLock takes or converts h's lock on name if no other process conflicts: an exclusive lock needs
// no other holder, a shared one no other exclusive holder.
func (h *simLockHost) tryLock(l lockHandle, exclusive bool) (bool, error) {
	name := l.(string)
	holders := h.table.holders[name]
	if holders == nil {
		holders = map[int]bool{}
		h.table.holders[name] = holders
	}
	for pid, ex := range holders {
		if pid != h.pidNum && (exclusive || ex) {
			return false, nil
		}
	}
	holders[h.pidNum] = exclusive
	h.table.polls = 0
	return true, nil
}

func (h *simLockHost) unlock(l lockHandle) error {
	delete(h.table.holders[l.(string)], h.pidNum)
	return nil
}

// now reads the simulated clock atomically: a client's connection goroutine reads it too.
func (h *simLockHost) now() time.Time { return time.UnixMicro(atomic.LoadInt64(h.table.clock)) }

func (h *simLockHost) pause() {
	t := h.table
	t.polls++
	if t.polls > simLockMaxPolls {
		panic(fmt.Sprintf("simulated deadlock: process p%d waited %d polls for a lock", h.pidNum, t.polls))
	}
	t.runProbes(h.pidNum)
	atomic.AddInt64(t.clock, simLockPoll)
}

func (h *simLockHost) pid() int { return h.pidNum }
