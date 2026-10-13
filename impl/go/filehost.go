package jed

// The file host: the filesystem operations a file database needs beyond its own byte device — host
// compaction's sibling temp file, the rename over the real path, and the directory barrier that
// makes the rename durable (spec/design/api.md §2.6). Production uses the OS. The deterministic
// simulator (spec/design/simulation-testing.md §14 S4) substitutes a simulated filesystem, so a
// crash can land between any two of these steps.

import (
	"os"
	"path/filepath"
)

type fileHost interface {
	// realPath resolves symlinks, so a rename replaces the real file, never a symlink naming it.
	realPath(path string) (string, error)
	// writeFile creates (or truncates) path, lets fill write it with positioned writes, gives it the
	// permission bits of the file at modeFrom, makes it durable unless noSync, and closes it. The
	// caller removes path on failure.
	writeFile(path, modeFrom string, noSync bool, fill func(writeAt func(off int64, p []byte) error) error) error
	rename(from, to string) error
	// remove deletes path, ignoring any error (cleanup after a failure).
	remove(path string)
	// syncDir makes dir's entries durable — best effort: not every platform can open a directory for
	// fsync (Windows), and the rename is already atomic there.
	syncDir(dir string)
	// openStore opens an existing file read-write as a database's byte device.
	openStore(path string, noSync bool) (blockStore, error)
}

// osFiles is the production file host.
var osFiles fileHost = osFileHost{}

type osFileHost struct{}

func (osFileHost) realPath(path string) (string, error) {
	real, err := filepath.EvalSymlinks(path)
	if err != nil {
		return "", ioError(err)
	}
	return real, nil
}

func (osFileHost) writeFile(path, modeFrom string, noSync bool, fill func(writeAt func(off int64, p []byte) error) error) error {
	f, err := os.Create(path)
	if err != nil {
		return ioError(err)
	}
	err = fill(func(off int64, p []byte) error {
		if _, werr := f.WriteAt(p, off); werr != nil {
			return ioError(werr)
		}
		return nil
	})
	if err == nil {
		var info os.FileInfo
		if info, err = os.Stat(modeFrom); err != nil {
			err = ioError(err)
		} else if cerr := f.Chmod(info.Mode().Perm()); cerr != nil {
			err = ioError(cerr)
		}
	}
	if err == nil && !noSync {
		if serr := f.Sync(); serr != nil {
			err = ioError(serr)
		}
	}
	if cerr := f.Close(); err == nil && cerr != nil {
		err = ioError(cerr)
	}
	return err
}

func (osFileHost) rename(from, to string) error {
	if err := os.Rename(from, to); err != nil {
		return ioError(err)
	}
	return nil
}

func (osFileHost) remove(path string) { _ = os.Remove(path) }

func (osFileHost) syncDir(dir string) {
	if d, err := os.Open(dir); err == nil {
		_ = d.Sync()
		_ = d.Close()
	}
}

func (osFileHost) openStore(path string, noSync bool) (blockStore, error) {
	f, err := os.OpenFile(path, os.O_RDWR, 0)
	if err != nil {
		return nil, ioError(err)
	}
	return &fileBlockStore{f: f, noSync: noSync}, nil
}
