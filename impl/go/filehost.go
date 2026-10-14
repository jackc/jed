package jed

// The file host: the filesystem operations a file database needs beyond its own byte device — create's
// and host compaction's sibling temp file, the rename over the real path, the directory barrier that
// makes the rename durable (spec/design/api.md §2.1, §2.6), and opening the file (main or an
// attachment). Production uses the OS. The deterministic simulator
// (spec/design/simulation-testing.md §14 S4) substitutes a simulated filesystem, so a crash can land
// between any two of these steps.

import (
	"errors"
	"io/fs"
	"os"
	"path/filepath"
)

type fileHost interface {
	// realPath resolves symlinks, so a rename replaces the real file, never a symlink naming it.
	realPath(path string) (string, error)
	// exists reports whether path names an existing file (create refuses one, 58P02).
	exists(path string) (bool, error)
	// writeFile creates (or truncates) path, lets fill write it with positioned writes, gives it the
	// permission bits of the file at modeFrom ("" keeps the default), makes it durable unless noSync,
	// and closes it. The caller removes path on failure.
	writeFile(path, modeFrom string, noSync bool, fill func(writeAt func(off int64, p []byte) error) error) error
	rename(from, to string) error
	// remove deletes path, ignoring any error (cleanup after a failure).
	remove(path string)
	// syncDir makes dir's entries durable — best effort: not every platform can open a directory for
	// fsync (Windows), and the rename is already atomic there.
	syncDir(dir string)
	// openStore opens an existing file as a database's byte device, read-write unless readOnly. A
	// missing file is 58P01.
	openStore(path string, noSync, readOnly bool) (blockStore, error)
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

func (osFileHost) exists(path string) (bool, error) {
	if _, err := os.Stat(path); err == nil {
		return true, nil
	} else if !errors.Is(err, fs.ErrNotExist) {
		return false, ioError(err)
	}
	return false, nil
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
	if err == nil && modeFrom != "" {
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

func (osFileHost) openStore(path string, noSync, readOnly bool) (blockStore, error) {
	// A read-only open never writes the file, so it is not opened for writing at all — the OS
	// enforces what the executor's 25006 guards promise (api.md §2.1).
	flag := os.O_RDWR
	if readOnly {
		flag = os.O_RDONLY
	}
	f, err := os.OpenFile(path, flag, 0)
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return nil, newError(UndefinedFile, "database file does not exist: "+path)
		}
		return nil, ioError(err)
	}
	return &fileBlockStore{f: f, noSync: noSync}, nil
}
