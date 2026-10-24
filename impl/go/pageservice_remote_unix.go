//go:build unix

package jed

// The stream transport's client (spec/design/page-service.md §2, §13; spec/pageservice/README.md
// §5.3): the same protocol over a child process's stdin and stdout instead of a local socket, which is
// how a client on another machine reaches an owner over SSH (`ssh host jed pages /path`).
//
// remoteLockHost runs the transport command once per connection. It never owns: there is no bundle to
// lock here, so the election always finds a serving owner and dials it by running the command again.
// Re-election and commit_status therefore work exactly as for a local client. Every hello carries the
// handle's credential, if it has one. The serving end (`jed pages`) is the Rust core's.

import (
	"errors"
	"io"
	"net"
	"os"
	"os/exec"
	"strings"
	"sync"
	"time"
	"unicode/utf8"
)

// remoteRetryInterval is how long the remote client waits between transport attempts.
const remoteRetryInterval = 250 * time.Millisecond

// remoteStderrTail is how much of the transport command's standard error a failed open reports.
const remoteStderrTail = 2048

// OpenRemote opens a database that another machine (or process) serves over a stream: a page client
// whose connection is the standard input and output of command, run once per connection
// (spec/pageservice/README.md §5.3). The CLI opens `ssh://host/path` as
// `[]string{"ssh", "--", "host", "jed pages '/path'"}`. name identifies the database in this process
// (one handle per name, like a path) and is what Path reports.
//
// The handle is an ordinary page client: it runs its own engine over pages fetched through the
// command, commits through it, and when the connection ends it runs the command again (re-election,
// README.md §6.5) — it never becomes the owner, whatever opts.Ownership says. opts.Credential unlocks
// an encrypted database here and is also sent in every connection's hello, for a serving end that must
// own the database (README.md §3.1). An open that cannot reach the database in opts.FileLockTimeoutMs
// fails 08006 with the command's standard error; an error the remote side reports (58P01, 0A000, …)
// is returned as is. Unix only.
func OpenRemote(command []string, name string, opts OpenOptions) (*Database, error) {
	if len(command) == 0 {
		return nil, newError(ConnectionFailure, "empty transport command")
	}
	host := &remoteLockHost{command: append([]string(nil), command...), credential: opts.Credential}
	opts.Ownership = OwnershipNever
	opts.Locking = LockingAuto
	if opts.Environment == nil {
		opts.Environment = NewEnvironment()
	}
	db, err := openServiceWith(newServiceHandoff(host, name, opts), true)
	if err != nil {
		return nil, host.openError(err)
	}
	return db, nil
}

// remoteLockHost is the page-service host of a remote database: every dial runs command, and the
// database is always owned elsewhere. Bundle locks are placeholders that are never contended.
type remoteLockHost struct {
	command []string
	// credential is sent in every connection's hello (README.md §5.3).
	credential *Credential
	// failure is the tail of the last transport's standard error, or why it could not run, for a
	// failed open's message.
	failureMu sync.Mutex
	failure   string
}

// remotePaths is the process-local duplicate-open registry of remote handles, by name.
var remotePaths = struct {
	sync.Mutex
	names map[string]bool
}{names: make(map[string]bool)}

func (*remoteLockHost) claimPath(name string) bool {
	remotePaths.Lock()
	defer remotePaths.Unlock()
	if remotePaths.names[name] {
		return false
	}
	remotePaths.names[name] = true
	return true
}

func (*remoteLockHost) releasePath(name string) {
	remotePaths.Lock()
	defer remotePaths.Unlock()
	delete(remotePaths.names, name)
}

// remoteLock is a bundle file's placeholder lock.
type remoteLock struct{}

func (*remoteLockHost) openBundle(string) (lockFiles, error) {
	return lockFiles{presence: remoteLock{}, arrival: remoteLock{}}, nil
}
func (*remoteLockHost) validateMarker(string) error { return nil }
func (*remoteLockHost) closeFiles(lockFiles)        {}

// tryLock grants arrival (shared) always and presence (exclusive) never: the database is owned on the
// remote side.
func (*remoteLockHost) tryLock(_ lockHandle, exclusive bool) (bool, error) { return !exclusive, nil }
func (*remoteLockHost) unlock(lockHandle) error                            { return nil }
func (*remoteLockHost) now() time.Time                                     { return time.Now() }
func (*remoteLockHost) pause()                                             { time.Sleep(remoteRetryInterval) }
func (*remoteLockHost) pid() int                                           { return os.Getpid() }

// readOwnerRecord always names a serving owner of this user, reached by running the command.
func (*remoteLockHost) readOwnerRecord(string) (ownerRecord, bool) {
	return ownerRecord{serving: true, uid: currentUID(), endpoint: "remote"}, true
}
func (*remoteLockHost) writeOwnerRecord(string, ownerRecord) error { return nil }
func (*remoteLockHost) removeOwnerRecord(string)                   {}
func (*remoteLockHost) endpointPath(string, uint32, id16) string   { return "" }
func (*remoteLockHost) listen(string) (net.Listener, error) {
	return nil, errors.New("a remote database is served elsewhere")
}
func (*remoteLockHost) removeEndpoint(string) {}

// helloCredential and readsAhead make the host a streamLockHost: its clients send the credential and
// read ahead, since their round trips cross a network (page-service.md §5.3).
func (h *remoteLockHost) helloCredential() *Credential { return h.credential }
func (*remoteLockHost) readsAhead() bool               { return true }

// dial runs the transport command; its stdin and stdout are the connection. The pipes are this
// process's own, so closing the connection never races the reap of the transport.
func (h *remoteLockHost) dial(string) (net.Conn, error) {
	var files []*os.File
	pipe := func() (r, w *os.File, err error) {
		if r, w, err = os.Pipe(); err == nil {
			files = append(files, r, w)
		}
		return r, w, err
	}
	closeAll := func() {
		for _, f := range files {
			_ = f.Close()
		}
	}
	childIn, in, err := pipe()
	if err != nil {
		closeAll()
		return nil, err
	}
	out, childOut, err := pipe()
	if err != nil {
		closeAll()
		return nil, err
	}
	errOut, childErr, err := pipe()
	if err != nil {
		closeAll()
		return nil, err
	}
	cmd := exec.Command(h.command[0], h.command[1:]...)
	cmd.Stdin, cmd.Stdout, cmd.Stderr = childIn, childOut, childErr
	h.setFailure("")
	if err := cmd.Start(); err != nil {
		closeAll()
		h.setFailure("could not run " + h.command[0] + ": " + err.Error())
		return nil, err
	}
	// The child holds its own ends now.
	_, _, _ = childIn.Close(), childOut.Close(), childErr.Close()
	go h.collectStderr(errOut)
	return &pipeConn{cmd: cmd, in: out, out: in}, nil
}

// collectStderr keeps the last remoteStderrTail bytes the transport writes to its standard error.
func (h *remoteLockHost) collectStderr(r io.ReadCloser) {
	defer r.Close()
	chunk := make([]byte, 1024)
	for {
		n, err := r.Read(chunk)
		if n > 0 {
			h.failureMu.Lock()
			text := h.failure + string(chunk[:n])
			if len(text) > remoteStderrTail {
				cut := len(text) - remoteStderrTail
				for cut < len(text) && !utf8.RuneStart(text[cut]) {
					cut++
				}
				text = text[cut:]
			}
			h.failure = text
			h.failureMu.Unlock()
		}
		if err != nil {
			return
		}
	}
}

func (h *remoteLockHost) setFailure(text string) {
	h.failureMu.Lock()
	h.failure = text
	h.failureMu.Unlock()
}

// openError words a failed open: an election that timed out without a connection reports the
// transport's own complaint (08006).
func (h *remoteLockHost) openError(err error) error {
	var engine *EngineError
	if !errors.As(err, &engine) || (engine.Code() != "55006" && engine.Code() != "08006") {
		return err
	}
	h.failureMu.Lock()
	detail := strings.TrimSpace(h.failure)
	h.failureMu.Unlock()
	message := "could not reach the database through `" + strings.Join(h.command, " ") + "`"
	if detail != "" {
		message += ": " + detail
	}
	return newError(ConnectionFailure, message)
}

// pipeConn is one connection over a transport process: reads come from its stdout, writes go to its
// stdin. Closing closes both pipes, so a well-behaved transport exits on its own (one that does not
// is killed) and a read in progress returns. The page client uses only Read, Write, and Close.
type pipeConn struct {
	cmd       *exec.Cmd
	in        *os.File
	out       *os.File
	closeOnce sync.Once
}

func (c *pipeConn) Read(p []byte) (int, error)  { return c.in.Read(p) }
func (c *pipeConn) Write(p []byte) (int, error) { return c.out.Write(p) }

func (c *pipeConn) Close() error {
	c.closeOnce.Do(func() {
		_ = c.out.Close()
		_ = c.in.Close()
		go c.reap()
	})
	return nil
}

// reap waits briefly for the transport to exit after its input closed, then kills it.
func (c *pipeConn) reap() {
	exited := make(chan struct{})
	go func() {
		_ = c.cmd.Wait()
		close(exited)
	}()
	select {
	case <-exited:
	case <-time.After(2 * time.Second):
		_ = c.cmd.Process.Kill()
		<-exited
	}
}

func (c *pipeConn) LocalAddr() net.Addr              { return pipeAddr{} }
func (c *pipeConn) RemoteAddr() net.Addr             { return pipeAddr{} }
func (c *pipeConn) SetDeadline(time.Time) error      { return errors.ErrUnsupported }
func (c *pipeConn) SetReadDeadline(time.Time) error  { return errors.ErrUnsupported }
func (c *pipeConn) SetWriteDeadline(time.Time) error { return errors.ErrUnsupported }

type pipeAddr struct{}

func (pipeAddr) Network() string { return "pipe" }
func (pipeAddr) String() string  { return "transport" }
