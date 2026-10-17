//go:build unix

package jed

// The page-service endpoint and owner record on the host filesystem (spec/pageservice/README.md §5):
// where an owner binds its socket, how a directory proves it is private to one user, and the owner
// record clients read to find the endpoint. Unix only: Windows serving is not yet specified
// (page-service.md §14), so `locking = service` fails 0A000 there.

import (
	"encoding/hex"
	"errors"
	"net"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"syscall"
)

func pageServiceSupported() bool { return true }

func currentUID() uint32 { return uint32(os.Getuid()) }

// privateDir reports whether dir is a real directory (not a symlink) owned by uid with mode exactly
// 0700 — the check every endpoint directory must pass (README.md §5.2).
func privateDir(dir string, uid uint32) bool {
	info, err := os.Lstat(dir)
	special := os.ModeSymlink | os.ModeSetuid | os.ModeSetgid | os.ModeSticky
	if err != nil || !info.IsDir() || info.Mode()&special != 0 || info.Mode().Perm() != 0o700 {
		return false
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	return ok && stat.Uid == uid
}

// ensurePrivateDir creates dir with mode 0700 when it is absent, then checks it.
func ensurePrivateDir(dir string, uid uint32) bool {
	if err := os.Mkdir(dir, 0o700); err != nil && !errors.Is(err, os.ErrExist) {
		return false
	}
	return privateDir(dir, uid)
}

// endpointPath picks the owner's socket path (README.md §5.2): the bundle's per-user directory when
// the path fits, otherwise a runtime directory. "" means the owner cannot serve.
func endpointPath(dbPath string, uid uint32, incarnation [16]byte) string {
	bundleDir := filepath.Join(dbPath+".lock", "uid-"+strconv.FormatUint(uint64(uid), 10))
	if path := filepath.Join(bundleDir, "endpoint"); len(path) <= psEndpointMaxPathBytes && ensurePrivateDir(bundleDir, uid) {
		return path
	}
	name := "jed-" + hex.EncodeToString(incarnation[:8]) + ".sock"
	var candidates []string
	switch runtime.GOOS {
	case "linux":
		if dir := os.Getenv("XDG_RUNTIME_DIR"); dir != "" {
			candidates = append(candidates, dir)
		}
	case "darwin":
		if dir := os.Getenv("TMPDIR"); dir != "" {
			candidates = append(candidates, filepath.Clean(dir))
		}
	}
	for _, dir := range candidates {
		if path := filepath.Join(dir, name); len(path) <= psEndpointMaxPathBytes && privateDir(dir, uid) {
			return path
		}
	}
	dir := "/tmp/jed-" + strconv.FormatUint(uint64(uid), 10)
	if path := filepath.Join(dir, name); len(path) <= psEndpointMaxPathBytes && ensurePrivateDir(dir, uid) {
		return path
	}
	return ""
}

// listenEndpoint binds the owner's socket at path, replacing a stale one. Only a presence-EX holder
// calls it, so a file already at path is a dead owner's endpoint.
func listenEndpoint(path string) (net.Listener, error) {
	if err := os.Remove(path); err != nil && !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	l, err := net.Listen("unix", path)
	if err != nil {
		return nil, err
	}
	// The listener must not unlink the path on Close: the owner removes it explicitly, before it
	// releases presence, so it never removes a successor's socket.
	l.(*net.UnixListener).SetUnlinkOnClose(false)
	return l, nil
}

func dialEndpoint(path string) (net.Conn, error) { return net.Dial("unix", path) }

// ownerRecordPath is the owner record's place in the bundle (README.md §5.1).
func ownerRecordPath(dbPath string) string { return filepath.Join(dbPath+".lock", "owner") }

// writeOwnerRecord publishes r atomically: a temp file named by the incarnation, then a rename.
func writeOwnerRecord(dbPath string, r ownerRecord) error {
	final := ownerRecordPath(dbPath)
	tmp := final + ".tmp-" + hex.EncodeToString(r.incarnation[:])
	if err := os.WriteFile(tmp, encodeOwnerRecord(r), 0o644); err != nil {
		return newError(IoError, "write owner record: "+err.Error())
	}
	if err := os.Rename(tmp, final); err != nil {
		_ = os.Remove(tmp)
		return newError(IoError, "publish owner record: "+err.Error())
	}
	return nil
}

// readOwnerRecord returns the current owner record, or ok=false when it is absent or unparsable
// (README.md §5.1: such a record is treated as absent).
func readOwnerRecord(dbPath string) (ownerRecord, bool) {
	b, err := os.ReadFile(ownerRecordPath(dbPath))
	if err != nil {
		return ownerRecord{}, false
	}
	r, err := decodeOwnerRecord(b)
	if err != nil {
		return ownerRecord{}, false
	}
	return r, true
}
