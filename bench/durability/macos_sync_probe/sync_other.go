//go:build !darwin

package main

import (
	"errors"
	"os"
)

const supportedPlatform = false

func platformFlush(_ *os.File, _ flushKind) error {
	return errors.New("macOS flushing is unavailable on this platform")
}
