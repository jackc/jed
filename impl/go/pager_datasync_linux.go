//go:build linux

package jed

import (
	"os"
	"syscall"
)

// datasync flushes the file's data (plus the metadata needed to retrieve it) without the extra inode
// metadata a full fsync forces (mtime/ctime) — i.e. fdatasync. On Linux it is syscall.Fdatasync, pure
// Go with no cgo or FFI (CLAUDE.md §2), keeping the core memory-safe. It is the metadata-free barrier
// pager.sync uses per commit: an overwrite into the preallocated region (pager.reserve) then flushes
// only data, with no file-size/inode-timestamp journal (spec/design/pager.md §7, the durable-commit
// win). The portable fallback for other platforms is pager_datasync_other.go.
func datasync(f *os.File) error {
	return syscall.Fdatasync(int(f.Fd()))
}
