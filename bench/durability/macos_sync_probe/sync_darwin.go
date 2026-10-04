//go:build darwin

package main

import (
	"fmt"
	"os"
	"syscall"
)

const supportedPlatform = true

func platformFlush(file *os.File, kind flushKind) error {
	var trap, command uintptr
	switch kind {
	case dataFlush:
		trap = syscall.SYS_FDATASYNC
	case fileFlush:
		trap = syscall.SYS_FSYNC
	case fullFlush:
		trap, command = syscall.SYS_FCNTL, syscall.F_FULLFSYNC
	case barrierFlush:
		// Apple's public fcntl.h defines 85; Go's frozen syscall constants
		// omit it. No pointers, unsafe, cgo, FFI, or dependencies are needed.
		trap, command = syscall.SYS_FCNTL, 85
	default:
		return fmt.Errorf("unknown flush %q", kind)
	}
	conn, err := file.SyscallConn()
	if err != nil {
		return err
	}
	var callErr error
	// Control keeps the descriptor alive even if another goroutine closes File.
	err = conn.Control(func(fd uintptr) {
		for {
			_, _, errno := syscall.Syscall(trap, fd, command, 0)
			if errno == syscall.EINTR {
				continue
			}
			if errno != 0 {
				callErr = errno
			}
			return
		}
	})
	if err != nil {
		return err
	}
	return callErr
}
