package jed

// The coordination host: everything the shared-file coordinator (coordinator.go, spec/design/
// locking.md) needs from its environment — the lock bundle beside the database file, whole-file
// locks on it, the clock its deadlines read, the pause between polls of a contended lock, and the process-local duplicate-open registry. Production uses the OS.
// The deterministic simulator (spec/design/simulation-testing.md §14 S5) substitutes simulated
// processes sharing one lock table, so the protocol runs, crashes, and interleaves deterministically.

import (
	"os"
	"sync"
	"time"
)

// lockHandle is one bundle file's lock: an *os.File for the OS host.
type lockHandle any

type lockHost interface {
	// claimPath registers path as open in this process; false if it already is.
	claimPath(path string) bool
	releasePath(path string)
	openBundle(path string) (lockFiles, error)
	validateMarker(path string) error
	closeFiles(files lockFiles)
	tryLock(h lockHandle, exclusive bool) (bool, error)
	unlock(h lockHandle) error
	now() time.Time
	// pause waits between polls of a contended lock.
	pause()
	pid() int
}

// osLocks is the production coordination host.
var osLocks lockHost = osLockHost{}

type osLockHost struct{}

var processOpenPaths = struct {
	sync.Mutex
	paths map[string]bool
}{paths: make(map[string]bool)}

func (osLockHost) claimPath(path string) bool {
	processOpenPaths.Lock()
	defer processOpenPaths.Unlock()
	if processOpenPaths.paths[path] {
		return false
	}
	processOpenPaths.paths[path] = true
	return true
}

func (osLockHost) releasePath(path string) {
	processOpenPaths.Lock()
	delete(processOpenPaths.paths, path)
	processOpenPaths.Unlock()
}

func (osLockHost) openBundle(path string) (lockFiles, error) { return openLockBundle(path) }
func (osLockHost) validateMarker(path string) error          { return validateProtocolMarker(path) }
func (osLockHost) closeFiles(files lockFiles)                { closeOSLockFiles(files) }

func (osLockHost) tryLock(h lockHandle, exclusive bool) (bool, error) {
	return osTryLock(h.(*os.File), exclusive)
}

func (osLockHost) unlock(h lockHandle) error { return osUnlock(h.(*os.File)) }
func (osLockHost) now() time.Time            { return time.Now() }
func (osLockHost) pause()                    { time.Sleep(2 * time.Millisecond) }
func (osLockHost) pid() int                  { return os.Getpid() }

func closeOSLockFiles(f lockFiles) {
	for _, h := range []lockHandle{f.arrival, f.presence} {
		if file, ok := h.(*os.File); ok && file != nil {
			_ = file.Close()
		}
	}
}
