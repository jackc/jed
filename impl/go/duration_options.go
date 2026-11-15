package jed

import (
	"math"
	"time"
)

// durationMillis rounds a validated, nonnegative duration up to the engine's
// millisecond unit. A positive timeout must never become zero (unlimited).
func durationMillis(d time.Duration) uint64 {
	ms := uint64(d / time.Millisecond)
	if d%time.Millisecond != 0 {
		ms++
	}
	return ms
}

func durationMillisPtr(d *time.Duration) *uint64 {
	if d == nil {
		return nil
	}
	ms := durationMillis(*d)
	return &ms
}

// millisDuration saturates values beyond Go's duration range. SQL and protocol
// settings use uint64 milliseconds, which can exceed that range.
func millisDuration(ms uint64) time.Duration {
	if ms > uint64(math.MaxInt64/int64(time.Millisecond)) {
		return time.Duration(math.MaxInt64)
	}
	return time.Duration(ms) * time.Millisecond
}

func validateDuration(name string, d time.Duration) error {
	if d < 0 {
		return newError(InvalidParameterValue, name+" must not be negative")
	}
	return nil
}

func validateDurationOptions(flush, file, idleTx, idle, yield, shutdown *time.Duration) error {
	for _, option := range []struct {
		name  string
		value *time.Duration
	}{
		{"FlushInterval", flush},
		{"FileLockTimeout", file},
		{"IdleInTransactionTimeout", idleTx},
		{"IdleSessionTimeout", idle},
		{"YieldGrace", yield},
		{"ShutdownGrace", shutdown},
	} {
		if option.value != nil {
			if err := validateDuration(option.name, *option.value); err != nil {
				return err
			}
		}
	}
	return nil
}

func (o OpenOptions) validateDurations() error {
	return validateDurationOptions(o.FlushInterval, o.FileLockTimeout,
		o.IdleInTransactionTimeout, o.IdleSessionTimeout, o.YieldGrace, o.ShutdownGrace)
}
