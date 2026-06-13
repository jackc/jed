//go:build !linux

package jed

import "os"

// datasync falls back to a full fsync on platforms without a stdlib fdatasync wrapper. Still correct
// (the commit is durable) — it just forgoes the metadata-free optimization the Linux path gets, so a
// growing-file commit there keeps paying the inode-metadata flush (spec/design/pager.md §7). The
// preallocation in pager.reserve still helps by keeping most commits' writes in-region.
func datasync(f *os.File) error {
	return f.Sync()
}
