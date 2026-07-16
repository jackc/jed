//go:build unix

package jed

import (
	"errors"
	"os"
	"syscall"
)

var errLockUnsupported = syscall.ENOSYS

func osLocksSupported() bool { return true }

func osTryLock(file *os.File, exclusive bool) (bool, error) {
	op := syscall.LOCK_SH | syscall.LOCK_NB
	if exclusive {
		op = syscall.LOCK_EX | syscall.LOCK_NB
	}
	err := syscall.Flock(int(file.Fd()), op)
	if err == nil {
		return true, nil
	}
	if errors.Is(err, syscall.EWOULDBLOCK) || errors.Is(err, syscall.EAGAIN) {
		return false, nil
	}
	return false, err
}

func osUnlock(file *os.File) error { return syscall.Flock(int(file.Fd()), syscall.LOCK_UN) }

func hasMultipleLinks(path string) (bool, error) {
	info, err := os.Stat(path)
	if err != nil {
		return false, err
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok {
		return false, errors.New("filesystem link count unavailable")
	}
	return stat.Nlink != 1, nil
}
