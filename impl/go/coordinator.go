package jed

// Stable cross-process file coordination (spec/design/locking.md). The database inode is never the
// lock identity: five persistent empty files beside it carry whole-file kernel locks.

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
	protocolMarker           = "protocol-v1"
)

type leaseState uint32

const (
	leaseAlone leaseState = iota + 1
	leaseShared
	leaseExclusive
	leasePoisoned
)

type lockFiles struct {
	presence, arrival, transition, writer, commit lockHandle
}

type fileCoordinator struct {
	host    lockHost
	path    string
	files   lockFiles
	state   atomic.Uint32
	pid     int
	stop    chan struct{}
	done    chan struct{}
	started atomic.Bool
	once    sync.Once
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
	closeFiles := true
	defer func() {
		if closeFiles {
			host.closeFiles(files)
		}
	}()
	deadline := fileLockDeadline(host, timeoutOpt)
	if err := lockUntil(host, files.arrival, false, deadline, path, false); err != nil {
		return fail(err)
	}
	if err := host.validateMarker(path); err != nil {
		_ = host.unlock(files.arrival)
		return fail(err)
	}

	state := leaseShared
	if mode == LockingExclusive {
		for {
			if err := lockUntil(host, files.transition, true, deadline, path, false); err != nil {
				_ = host.unlock(files.arrival)
				return fail(err)
			}
			ok, lockErr := host.tryLock(files.presence, true)
			_ = host.unlock(files.transition)
			if lockErr != nil {
				_ = host.unlock(files.arrival)
				return fail(classifyLockError(lockErr))
			}
			if ok {
				state = leaseExclusive
				break
			}
			if err := checkLockDeadline(host, deadline, path, false); err != nil {
				_ = host.unlock(files.arrival)
				return fail(err)
			}
			host.pause()
		}
	} else {
		if err := lockUntil(host, files.transition, true, deadline, path, false); err != nil {
			_ = host.unlock(files.arrival)
			return fail(err)
		}
		alone, lockErr := host.tryLock(files.presence, true)
		_ = host.unlock(files.transition)
		if lockErr != nil {
			_ = host.unlock(files.arrival)
			return fail(classifyLockError(lockErr))
		}
		if alone {
			state = leaseAlone
		} else if err := lockUntil(host, files.presence, false, deadline, path, false); err != nil {
			_ = host.unlock(files.arrival)
			return fail(err)
		}
	}
	_ = host.unlock(files.arrival)
	c := &fileCoordinator{host: host, path: path, files: files, pid: host.pid(), stop: make(chan struct{}), done: make(chan struct{})}
	c.state.Store(uint32(state))
	closeFiles = false
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

func (c *fileCoordinator) startProbe(tick func()) {
	c.started.Store(true)
	if c.lease() == leaseExclusive {
		close(c.done)
		return
	}
	c.host.startProbe(c, tick)
}

func (c *fileCoordinator) close() {
	c.once.Do(func() {
		close(c.stop)
		if c.started.Load() {
			<-c.done
		}
		c.host.closeFiles(c.files)
		c.host.releasePath(c.path)
	})
}

func (c *fileCoordinator) lockCommitShared() error {
	return lockBlocking(c.host, c.files.commit, false, c.path)
}

func (c *fileCoordinator) lockCommitExclusive() error {
	return lockBlocking(c.host, c.files.commit, true, c.path)
}
func (c *fileCoordinator) unlockCommit() { _ = c.host.unlock(c.files.commit) }

func (c *fileCoordinator) lockWriter(timeoutMs uint64) error {
	var deadline time.Time
	if timeoutMs != 0 {
		deadline = c.host.now().Add(time.Duration(timeoutMs) * time.Millisecond)
	}
	return lockUntil(c.host, c.files.writer, true, deadline, c.path, true)
}

func (c *fileCoordinator) unlockWriter() { _ = c.host.unlock(c.files.writer) }

func (c *fileCoordinator) tryArrivalExclusive() (bool, error) {
	return c.host.tryLock(c.files.arrival, true)
}

func (c *fileCoordinator) unlockArrival() { _ = c.host.unlock(c.files.arrival) }
func (c *fileCoordinator) lockTransition() error {
	return lockBlocking(c.host, c.files.transition, true, c.path)
}
func (c *fileCoordinator) unlockTransition() { _ = c.host.unlock(c.files.transition) }

func (c *fileCoordinator) downgradePresence() error {
	c.setLease(leaseShared)
	if err := c.host.unlock(c.files.presence); err != nil {
		return classifyLockError(err)
	}
	return lockBlocking(c.host, c.files.presence, false, c.path)
}

func (c *fileCoordinator) tryUpgradePresence() (bool, error) {
	if err := c.host.unlock(c.files.presence); err != nil {
		return false, classifyLockError(err)
	}
	ok, err := c.host.tryLock(c.files.presence, true)
	if err != nil {
		return false, classifyLockError(err)
	}
	if ok {
		return true, nil
	}
	if err := lockBlocking(c.host, c.files.presence, false, c.path); err != nil {
		return false, err
	}
	return false, nil
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
	for _, name := range []string{protocolMarker, "presence", "arrival", "transition", "writer", "commit"} {
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
	if opened.transition, err = open("transition"); err != nil {
		closeOSLockFiles(opened)
		return lockFiles{}, newError(IoError, err.Error())
	}
	if opened.writer, err = open("writer"); err != nil {
		closeOSLockFiles(opened)
		return lockFiles{}, newError(IoError, err.Error())
	}
	if opened.commit, err = open("commit"); err != nil {
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
		if strings.HasPrefix(entry.Name(), "protocol-v") && entry.Name() != protocolMarker {
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

func resolvedLocking(mode Locking) (Locking, error) {
	if mode == LockingAuto {
		mode = LockingShared
	}
	if mode > LockingNone {
		return 0, newError(FeatureNotSupported, "unknown file locking mode")
	}
	if !osLocksSupported() && mode != LockingNone {
		return 0, newError(FeatureNotSupported, "file locking is unavailable on "+runtime.GOOS)
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
