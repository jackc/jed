package jed

// Election at open under the test-only LockingService (spec/design/page-service.md §4.3): the opener
// that takes presence EX owns the database and serves it; every other opener connects to the owner's
// endpoint as a read-only page client (phase 2). The OS lock is the only authority — the owner record
// only tells a client where to connect and words its timeout error.

import (
	"os"
	"path/filepath"
)

// serviceElection is the outcome of an open-time election: exactly one of owner and client is set.
type serviceElection struct {
	owner  *fileCoordinator
	client *psClient
	path   string
}

// electService runs the election for the database at path (already resolved as prepareOpenCoordinator
// resolves it). It retries until the open timeout: presence may free up, and a new owner may not have
// bound its endpoint yet.
func electService(host lockHost, path string, timeoutOpt *uint64) (serviceElection, error) {
	if !host.claimPath(path) {
		return serviceElection{}, newError(ObjectInUse, "database is already open in this process: "+path+" (share one Database handle)")
	}
	files, err := host.openBundle(path)
	if err != nil {
		host.releasePath(path)
		return serviceElection{}, err
	}
	fail := func(err error) (serviceElection, error) {
		host.closeFiles(files)
		host.releasePath(path)
		return serviceElection{}, err
	}
	deadline := fileLockDeadline(host, timeoutOpt)
	if err := lockUntil(host, files.arrival, false, deadline, path, false); err != nil {
		return fail(err)
	}
	if err := host.validateMarker(path); err != nil {
		_ = host.unlock(files.arrival)
		return fail(err)
	}
	reason := ""
	for {
		owned, err := host.tryLock(files.presence, true)
		if err != nil {
			_ = host.unlock(files.arrival)
			return fail(classifyLockError(err))
		}
		if owned {
			_ = host.unlock(files.arrival)
			c := &fileCoordinator{host: host, path: path, files: files, pid: host.pid(), stop: make(chan struct{}), done: make(chan struct{})}
			c.setLease(leaseServiceOwner)
			return serviceElection{owner: c, path: path}, nil
		}
		record, ok := readOwnerRecord(path)
		switch {
		case !ok:
			reason = ""
		case record.uid != currentUID():
			reason = "owned by another user's process"
		case !record.serving:
			reason = "owned by a non-serving process"
		default:
			reason = ""
			cl, retry, err := connectPageClient(path, record.endpoint)
			if err == nil {
				// A client holds no bundle lock while connected (page-service.md §4.2).
				_ = host.unlock(files.arrival)
				host.closeFiles(files)
				cl.release = func() { host.releasePath(path) }
				return serviceElection{client: cl, path: path}, nil
			}
			if !retry {
				_ = host.unlock(files.arrival)
				return fail(err)
			}
		}
		if err := checkLockDeadline(host, deadline, path, false); err != nil {
			_ = host.unlock(files.arrival)
			if reason != "" {
				err = newError(ObjectInUse, "database file is in use: "+path+" ("+reason+")")
			}
			return fail(err)
		}
		host.pause()
	}
}

// resolveServicePath resolves an open path as prepareOpenCoordinator does (symlinks, hard links).
func resolveServicePath(path string) (string, error) {
	real, err := filepath.EvalSymlinks(path)
	if err != nil {
		if os.IsNotExist(err) {
			return "", newError(UndefinedFile, "database file does not exist: "+path)
		}
		return "", newError(IoError, "resolve database path: "+err.Error())
	}
	if real, err = filepath.Abs(real); err != nil {
		return "", newError(IoError, "resolve database path: "+err.Error())
	}
	multiple, err := hasMultipleLinks(real)
	if err != nil {
		return "", newError(IoError, "inspect database identity: "+err.Error())
	}
	if multiple {
		return "", newError(ObjectInUse, "hard-linked database paths cannot use jed locking: "+real)
	}
	return real, nil
}

// openServiceDatabase opens path under LockingService: as the serving owner, or as a page client.
func openServiceDatabase(path string, opts OpenOptions) (*Database, error) {
	if _, err := resolvedLocking(LockingService); err != nil {
		return nil, err
	}
	real, err := resolveServicePath(path)
	if err != nil {
		return nil, err
	}
	election, err := electService(osLocks, real, opts.FileLockTimeoutMs)
	if err != nil {
		return nil, err
	}
	env := opts.Environment
	if env == nil {
		env = NewEnvironment()
	}
	if election.client != nil {
		e, err := openPageClient(election.client, opts, env)
		if err != nil {
			election.client.close()
			return nil, err
		}
		e.spill = osSpillHost{dir: os.TempDir()}
		c := sharedCoreFromEngineCoordinated(e, nil)
		c.client = election.client
		return databaseOver(c), nil
	}
	opts.Locking = LockingNone
	opts.Environment = env
	e, err := openWithOptions(real, opts)
	if err != nil {
		election.owner.close()
		return nil, err
	}
	return startServiceOwner(e, election.owner, real, opts.MaxStorageBytes)
}

// startServiceOwner lifts an opened or created engine into a serving owner's shared core.
func startServiceOwner(e *engine, coord *fileCoordinator, real string, maxStorageBytes int64) (*Database, error) {
	c := sharedCoreFromEngineCoordinated(e, coord)
	c.storage.budget.limit = maxStorageBytes
	service, err := startPageServiceOwner(c, real)
	if err != nil {
		db := databaseOver(c)
		_ = db.Close()
		return nil, err
	}
	c.service = service
	return databaseOver(c), nil
}
