//go:build windows

package jed

import (
	"errors"
	"os"
	"runtime"
	"syscall"
	"unsafe"
)

const (
	lockfileFailImmediately = 0x00000001
	lockfileExclusiveLock   = 0x00000002
	allLockBytes            = ^uint32(0)
	errorLockViolation      = syscall.Errno(33)
	errorNotSupported       = syscall.Errno(50)
)

var (
	kernel32           = syscall.NewLazyDLL("kernel32.dll")
	procLockFileEx     = kernel32.NewProc("LockFileEx")
	procUnlockFileEx   = kernel32.NewProc("UnlockFileEx")
	errLockUnsupported = errorNotSupported
)

func osLocksSupported() bool { return true }

func osTryLock(file *os.File, exclusive bool) (bool, error) {
	flags := uintptr(lockfileFailImmediately)
	if exclusive {
		flags |= lockfileExclusiveLock
	}
	overlapped := new(syscall.Overlapped)
	r1, _, callErr := syscall.SyscallN(
		procLockFileEx.Addr(),
		file.Fd(),
		flags,
		0,
		uintptr(allLockBytes),
		uintptr(allLockBytes),
		uintptr(unsafe.Pointer(overlapped)),
	)
	runtime.KeepAlive(file)
	runtime.KeepAlive(overlapped)
	if r1 != 0 {
		return true, nil
	}
	if errors.Is(callErr, errorLockViolation) || errors.Is(callErr, syscall.ERROR_IO_PENDING) {
		return false, nil
	}
	return false, callErr
}

func osUnlock(file *os.File) error {
	overlapped := new(syscall.Overlapped)
	r1, _, callErr := syscall.SyscallN(
		procUnlockFileEx.Addr(),
		file.Fd(),
		0,
		uintptr(allLockBytes),
		uintptr(allLockBytes),
		uintptr(unsafe.Pointer(overlapped)),
	)
	runtime.KeepAlive(file)
	runtime.KeepAlive(overlapped)
	if r1 == 0 {
		return callErr
	}
	return nil
}

func hasMultipleLinks(path string) (bool, error) {
	file, err := os.Open(path)
	if err != nil {
		return false, err
	}
	defer file.Close()
	var info syscall.ByHandleFileInformation
	if err := syscall.GetFileInformationByHandle(syscall.Handle(file.Fd()), &info); err != nil {
		return false, err
	}
	return info.NumberOfLinks != 1, nil
}
