package jed

// Cross-process file coordination (spec/design/locking.md). The database inode is never the lock
// identity: persistent empty files in a bundle beside it carry whole-file kernel locks. presence EX is
// ownership: a page-service owner (page-service.md §4.1), an exclusive opener, or a file attachment
// holds it for its whole tenure; arrival SH guards the protocol-marker check.

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

const (
	defaultFileLockTimeoutMs = 5000
	// protocolMarker names the bundle's protocol: v2 is presence ownership under the page service.
	// legacyProtocolMarker (shared mode) may remain in an existing bundle; a v1 binary rejects the v2
	// marker, so the two never run on one file (page-service.md §12).
	protocolMarker       = "protocol-v2"
	legacyProtocolMarker = "protocol-v1"
)

type leaseState uint32

const (
	// leaseExclusive is a non-serving owner: an exclusive opener or a file attachment.
	leaseExclusive leaseState = iota + 1
	// leaseServiceOwner is the page-service owner (page-service.md §4.1), which also serves an endpoint.
	leaseServiceOwner
)

type lockFiles struct {
	presence, arrival lockHandle
}

type fileCoordinator struct {
	host  lockHost
	path  string
	files lockFiles
	state atomic.Uint32
	pid   int
	once  sync.Once
	// keepPath leaves the process-local path claim at close: a page-service owner's claim belongs to its
	// handle, which keeps the path through role changes (page-service.md §8).
	keepPath bool
	// record is set when this coordinator wrote a non-serving owner record, removed at close
	// (spec/pageservice/README.md §5.1).
	record bool
}

func prepareOpenCoordinator(path string, mode Locking, timeout *uint64) (*fileCoordinator, string, error) {
	mode, err := resolvedLocking(mode)
	if err != nil {
		return nil, "", err
	}
	if mode == LockingNone {
		return nil, path, nil
	}
	real, err := filepath.EvalSymlinks(path)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return nil, "", newError(UndefinedFile, "database file does not exist: "+path)
		}
		return nil, "", newError(IoError, "resolve database path: "+err.Error())
	}
	real, err = filepath.Abs(real)
	if err != nil {
		return nil, "", newError(IoError, "resolve database path: "+err.Error())
	}
	multiple, err := hasMultipleLinks(real)
	if err != nil {
		return nil, "", newError(IoError, "inspect database identity: "+err.Error())
	}
	if multiple {
		return nil, "", newError(ObjectInUse, "hard-linked database paths cannot use jed locking: "+real)
	}
	c, err := acquireCoordinator(osLocks, real, mode, timeout)
	return c, real, err
}

func prepareCreateCoordinator(path string, mode Locking, timeout *uint64) (*fileCoordinator, string, error) {
	mode, err := resolvedLocking(mode)
	if err != nil {
		return nil, "", err
	}
	if mode == LockingNone {
		return nil, path, nil
	}
	parent, err := filepath.EvalSymlinks(filepath.Dir(path))
	if err != nil {
		return nil, "", newError(IoError, "resolve database parent: "+err.Error())
	}
	parent, err = filepath.Abs(parent)
	if err != nil {
		return nil, "", newError(IoError, "resolve database parent: "+err.Error())
	}
	real := filepath.Join(parent, filepath.Base(path))
	c, err := acquireCoordinator(osLocks, real, mode, timeout)
	return c, real, err
}

// acquireCoordinator takes presence EX for path with the exclusive acquisition loop, until the open
// timeout (55006). A coordinator in mode LockingExclusive (an exclusive opener or a file attachment)
// writes a non-serving owner record, so a page-service opener words its timeout as "owned by a
// non-serving process"; a page-service creator writes its own record when it serves.
func acquireCoordinator(host lockHost, path string, mode Locking, timeoutOpt *uint64) (*fileCoordinator, error) {
	if !host.claimPath(path) {
		return nil, newError(ObjectInUse, "database is already open in this process: "+path+" (share one Database handle)")
	}
	fail := func(err error) (*fileCoordinator, error) {
		host.releasePath(path)
		return nil, err
	}
	files, err := host.openBundle(path)
	if err != nil {
		return fail(err)
	}
	failClosing := func(err error) (*fileCoordinator, error) {
		host.closeFiles(files)
		return fail(err)
	}
	deadline := fileLockDeadline(host, timeoutOpt)
	if err := lockUntil(host, files.arrival, false, deadline, path, false); err != nil {
		return failClosing(err)
	}
	if err := host.validateMarker(path); err != nil {
		_ = host.unlock(files.arrival)
		return failClosing(err)
	}
	if err := lockUntil(host, files.presence, true, deadline, path, false); err != nil {
		_ = host.unlock(files.arrival)
		reason := ""
		if record, ok := host.readOwnerRecord(path); ok {
			switch {
			case record.uid != currentUID():
				reason = "owned by another user's process"
			case !record.serving:
				reason = "owned by a non-serving process"
			}
		}
		if reason != "" {
			err = newError(ObjectInUse, "database file is in use: "+path+" ("+reason+")")
		}
		return failClosing(err)
	}
	_ = host.unlock(files.arrival)
	c := &fileCoordinator{host: host, path: path, files: files, pid: host.pid()}
	if mode == LockingExclusive {
		c.state.Store(uint32(leaseExclusive))
		c.record = host.writeOwnerRecord(path, ownerRecord{uid: currentUID(), incarnation: newIncarnation(), pid: uint32(os.Getpid())}) == nil
	} else {
		c.state.Store(uint32(leaseServiceOwner))
		c.keepPath = true
	}
	return c, nil
}

func (c *fileCoordinator) lease() leaseState     { return leaseState(c.state.Load()) }
func (c *fileCoordinator) setLease(s leaseState) { c.state.Store(uint32(s)) }

func (c *fileCoordinator) checkPID() error {
	if c.host.pid() != c.pid {
		return newError(ObjectInUse, "database handle was inherited across fork; reopen it in the child")
	}
	return nil
}

func (c *fileCoordinator) close() {
	c.once.Do(func() {
		if c.record {
			c.host.removeOwnerRecord(c.path)
		}
		c.host.closeFiles(c.files)
		if !c.keepPath {
			c.host.releasePath(c.path)
		}
	})
}

func openLockBundle(path string) (lockFiles, error) {
	bundle := path + ".lock"
	if err := os.Mkdir(bundle, 0o755); err != nil && !errors.Is(err, os.ErrExist) {
		return lockFiles{}, newError(IoError, "create coordination directory: "+err.Error())
	}
	info, err := os.Lstat(bundle)
	if err != nil || !info.IsDir() || info.Mode()&os.ModeSymlink != 0 {
		return lockFiles{}, newError(IoError, "coordination path is not a directory: "+bundle)
	}
	if err := validateProtocolMarker(path); err != nil {
		return lockFiles{}, err
	}
	for _, name := range []string{"presence", "arrival"} {
		if err := ensureRegularLockFile(filepath.Join(bundle, name)); err != nil {
			return lockFiles{}, err
		}
	}
	opened := lockFiles{}
	open := func(name string) (*os.File, error) {
		return os.Open(filepath.Join(bundle, name))
	}
	if opened.presence, err = open("presence"); err != nil {
		return lockFiles{}, newError(IoError, err.Error())
	}
	if opened.arrival, err = open("arrival"); err != nil {
		closeOSLockFiles(opened)
		return lockFiles{}, newError(IoError, err.Error())
	}
	return opened, nil
}

// validateProtocolMarker is run both while assembling the bundle and again after arrival SH is
// held. The second check is the protocol-migration barrier: a future cooperating migrator cannot
// change the version domain between validation and presence acquisition.
func validateProtocolMarker(path string) error {
	bundle := path + ".lock"
	entries, err := os.ReadDir(bundle)
	if err != nil {
		return newError(IoError, "read coordination directory: "+err.Error())
	}
	for _, entry := range entries {
		if name := entry.Name(); strings.HasPrefix(name, "protocol-v") && name != protocolMarker && name != legacyProtocolMarker {
			return newError(FeatureNotSupported, "unsupported coordination protocol marker "+entry.Name()+"; supported: "+protocolMarker)
		}
	}
	return ensureRegularLockFile(filepath.Join(bundle, protocolMarker))
}

func ensureRegularLockFile(path string) error {
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o644)
	if err == nil {
		_ = f.Close()
	} else if !errors.Is(err, os.ErrExist) {
		return newError(IoError, "create coordination entry: "+err.Error())
	}
	info, err := os.Lstat(path)
	if err != nil || !info.Mode().IsRegular() || info.Mode()&os.ModeSymlink != 0 {
		return newError(IoError, "coordination entry is not a regular file: "+path)
	}
	return nil
}

// resolvedLocking resolves a locking mode: auto is the page service where the host supports it
// (page-service.md §14), otherwise a non-serving owner (exclusive).
func resolvedLocking(mode Locking) (Locking, error) {
	if mode > LockingNone {
		return 0, newError(FeatureNotSupported, "unknown file locking mode")
	}
	if mode == LockingNone {
		return mode, nil
	}
	if !osLocksSupported() {
		return 0, newError(FeatureNotSupported, "file locking is unavailable on "+runtime.GOOS)
	}
	if mode == LockingAuto && !pageServiceSupported() {
		return LockingExclusive, nil
	}
	return mode, nil
}

func fileLockDeadline(host lockHost, value *uint64) time.Time {
	ms := uint64(defaultFileLockTimeoutMs)
	if value != nil {
		ms = *value
	}
	return host.now().Add(time.Duration(ms) * time.Millisecond)
}

func lockBlocking(host lockHost, h lockHandle, exclusive bool, path string) error {
	for {
		ok, err := host.tryLock(h, exclusive)
		if err != nil {
			return classifyLockError(err)
		}
		if ok {
			return nil
		}
		host.pause()
	}
}

func lockUntil(host lockHost, h lockHandle, exclusive bool, deadline time.Time, path string, writer bool) error {
	for {
		ok, err := host.tryLock(h, exclusive)
		if err != nil {
			return classifyLockError(err)
		}
		if ok {
			return nil
		}
		if err := checkLockDeadline(host, deadline, path, writer); err != nil {
			return err
		}
		host.pause()
	}
}

func checkLockDeadline(host lockHost, deadline time.Time, path string, writer bool) error {
	if deadline.IsZero() || host.now().Before(deadline) {
		return nil
	}
	if writer {
		return newError(LockNotAvailable, "could not obtain the database writer lock")
	}
	return newError(ObjectInUse, "database file is in use: "+path)
}

func classifyLockError(err error) error {
	if errors.Is(err, errLockUnsupported) {
		return newError(FeatureNotSupported, "OS file locks are unavailable")
	}
	return newError(IoError, fmt.Sprintf("file lock failed: %v", err))
}
