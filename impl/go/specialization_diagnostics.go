//go:build jed_specialization && jed_specialization_diagnostics

package jed

import "sync/atomic"

// Diagnostics are a separate, untimed build: no counter in benchmark binaries.
var specializationInvocations atomic.Uint64

func specializationInvoked() { specializationInvocations.Add(1) }
