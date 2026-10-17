package jed

// Election under the page service (LockingAuto) (spec/design/page-service.md §4.3; spec/pageservice/
// README.md §6.1, §6.5): the opener that takes presence EX owns the database and serves it; every other
// opener connects to the owner's endpoint as a page client. A client whose connection ends runs the
// same election again (re-election). The OS lock is the only authority — the owner record only tells
// a client where to connect and words its timeout error.

import (
	"errors"
	"os"
	"path/filepath"
	"time"
)

// serviceElection is the outcome of an election: exactly one of owner and client is set. A client
// arrives opened: clientEngine is its engine over the connection (openPageClient).
type serviceElection struct {
	owner        *fileCoordinator
	client       *psClient
	clientEngine *engine
}

// electOptions shape one election: never skips presence tries (OwnershipNever), pauseUntil skips them
// until then (the handoff pause), and reelect marks a re-election, which keeps the handle's existing
// process-local path claim. opts and env open the client's engine.
type electOptions struct {
	never      bool
	pauseUntil time.Time
	reelect    bool
	opts       OpenOptions
	env        *Environment
}

// electService runs the election for the database at path (already resolved as prepareOpenCoordinator
// resolves it). It retries until the open timeout: presence may free up, a new owner may not have bound
// its endpoint yet, and a draining owner closes the connection of a client that opened during the drain.
func electService(host lockHost, path string, timeoutOpt *uint64, readOnly bool, eo electOptions) (serviceElection, error) {
	if !eo.reelect && !host.claimPath(path) {
		return serviceElection{}, newError(ObjectInUse, "database is already open in this process: "+path+" (share one Database handle)")
	}
	files, err := host.openBundle(path)
	if err != nil {
		if !eo.reelect {
			host.releasePath(path)
		}
		return serviceElection{}, err
	}
	fail := func(err error) (serviceElection, error) {
		host.closeFiles(files)
		if !eo.reelect {
			host.releasePath(path)
		}
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
		if !eo.never && !host.now().Before(eo.pauseUntil) {
			owned, err := host.tryLock(files.presence, true)
			if err != nil {
				_ = host.unlock(files.arrival)
				return fail(classifyLockError(err))
			}
			if owned {
				_ = host.unlock(files.arrival)
				c := &fileCoordinator{host: host, path: path, files: files, pid: host.pid()}
				c.setLease(leaseServiceOwner)
				c.keepPath = true
				return serviceElection{owner: c}, nil
			}
		}
		record, ok := host.readOwnerRecord(path)
		switch {
		case !ok:
			reason = ""
		case record.uid != currentUID():
			reason = "owned by another user's process"
		case !record.serving:
			reason = "owned by a non-serving process"
		default:
			reason = ""
			cl, retry, err := connectPageClient(host, path, record.endpoint, readOnly)
			if err == nil {
				e, err := openPageClient(cl, eo.opts, eo.env)
				if err == nil {
					// A client holds no bundle lock while connected (page-service.md §4.2).
					_ = host.unlock(files.arrival)
					host.closeFiles(files)
					return serviceElection{client: cl, clientEngine: e}, nil
				}
				cl.close()
				if !isConnectionFailure(err) {
					_ = host.unlock(files.arrival)
					return fail(err)
				}
			} else if !retry || (isTooManyConnections(err) && !eo.reelect) {
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

func isTooManyConnections(err error) bool {
	var engine *EngineError
	return errors.As(err, &engine) && engine.Code() == "53300"
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

// openServiceDatabase opens path under the page service (LockingAuto): as the serving owner, or as a
// page client.
func openServiceDatabase(path string, opts OpenOptions) (*Database, error) {
	real, err := resolveServicePath(path)
	if err != nil {
		return nil, err
	}
	if opts.Environment == nil {
		opts.Environment = NewEnvironment()
	}
	return openServiceWith(newServiceHandoff(osLocks, real, opts), true)
}

// openServiceWith runs the election for handoff h's database and opens the handle in the role it
// wins. claim is false when the caller already holds the process-local path claim.
func openServiceWith(h *serviceHandoff, claim bool) (*Database, error) {
	opts := h.opts
	election, err := electService(h.host, h.path, opts.FileLockTimeoutMs, opts.ReadOnly, electOptions{never: opts.Ownership == OwnershipNever, reelect: !claim, opts: opts, env: opts.Environment})
	if err != nil {
		return nil, err
	}
	if election.client != nil {
		e := election.clientEngine
		c := sharedCoreFromEngineCoordinated(e, nil)
		c.handoff = h
		h.attachHost(c)
		c.storage.budget.limit = opts.MaxStorageBytes
		r := &serviceRole{gen: 1, client: election.client}
		c.installRole(r, e)
		c.watchClient(r)
		c.requestOwnershipIfPreferred(election.client)
		return databaseOver(c), nil
	}
	ownerOpts := opts
	ownerOpts.Locking = LockingNone
	e, err := openVia(h.fileHost(), h.path, ownerOpts)
	if err != nil {
		election.owner.close()
		if claim {
			h.host.releasePath(h.path)
		}
		return nil, err
	}
	return startServiceOwner(e, election.owner, h)
}

// startServiceOwner lifts an opened or created engine into a serving owner's shared core.
func startServiceOwner(e *engine, coord *fileCoordinator, h *serviceHandoff) (*Database, error) {
	c := sharedCoreFromEngineCoordinated(e, nil)
	c.handoff = h
	h.attachHost(c)
	c.storage.budget.limit = h.opts.MaxStorageBytes
	o, err := newPageServiceOwner(c, e.paging, e.committed.txid, h, coord)
	if err != nil {
		_ = e.paging.close()
		coord.close()
		h.host.releasePath(h.path)
		return nil, err
	}
	c.installRole(&serviceRole{gen: 1, owner: o}, e)
	if err := o.start(); err != nil {
		db := databaseOver(c)
		_ = db.Close()
		return nil, err
	}
	return databaseOver(c), nil
}
