//go:build !unix && !windows

package jed

import (
	"errors"
	"os"
)

var errLockUnsupported = errors.New("file locks unsupported")

func osLocksSupported() bool                 { return false }
func osTryLock(*os.File, bool) (bool, error) { return false, errLockUnsupported }
func osUnlock(*os.File) error                { return errLockUnsupported }
func hasMultipleLinks(string) (bool, error)  { return false, nil }
