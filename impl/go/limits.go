package jed

import (
	"fmt"
	"math"
)

// Unlimited, given as a size or count limit, removes the limit. Every limit in the Go API
// follows one rule: a positive value is the limit, 0 selects the documented default, Unlimited
// sets no limit, and any other negative value fails with SQLSTATE 22023.
const Unlimited = -1

// hostLimit converts a host limit to its internal form: 0 selects def, Unlimited selects unl, a
// positive value is kept, and any other negative value is 22023.
func hostLimit(name string, v, def, unl int64) (int64, error) {
	switch {
	case v == 0:
		return def, nil
	case v == Unlimited:
		return unl, nil
	case v > 0:
		return v, nil
	}
	return 0, newError(InvalidParameterValue, fmt.Sprintf(
		"invalid parameter value: %s must be positive, 0 for the default, or Unlimited (got %d)", name, v))
}

func hostLimitInt(name string, v int, def, unl int64) (int, error) {
	n, err := hostLimit(name, int64(v), def, unl)
	return int(n), err
}

// reportLimit is the host form of an internal limit whose unlimited form is unl.
func reportLimit(v, unl int64) int64 {
	if v == unl {
		return Unlimited
	}
	return v
}

// normalizeLimits validates the open-time limits and converts those whose internal form differs:
// MaxStorageBytes and MaxConnections keep 0 as "no limit", CacheBytes keeps 0 as "default".
// WorkMem keeps its host form; the engine maps it when it opens (applyOpenWorkMem). Idempotent, so
// a page-service handoff that re-opens with the stored options sees the same values.
func (o *OpenOptions) normalizeLimits() error {
	var err error
	if o.MaxStorageBytes, err = hostLimit("MaxStorageBytes", o.MaxStorageBytes, 0, 0); err != nil {
		return err
	}
	if o.CacheBytes, err = hostLimitInt("CacheBytes", o.CacheBytes, 0, math.MaxInt); err != nil {
		return err
	}
	if _, err = hostLimitInt("WorkMem", o.WorkMem, 0, 0); err != nil {
		return err
	}
	if o.MaxConnections, err = hostLimitInt("MaxConnections", o.MaxConnections, 0, 0); err != nil {
		return err
	}
	return nil
}

// applyOpenWorkMem sets an opened engine's work-memory budget from OpenOptions.WorkMem (host form):
// 0 keeps the default, Unlimited never spills (the internal 0).
func applyOpenWorkMem(e *engine, workMem int) {
	switch workMem {
	case 0:
	case Unlimited:
		e.session.workMem = 0
	default:
		e.session.workMem = workMem
	}
}

func (o *CreateOptions) normalizeLimits() error {
	var err error
	if o.MaxStorageBytes, err = hostLimit("MaxStorageBytes", o.MaxStorageBytes, 0, 0); err != nil {
		return err
	}
	if o.MaxConnections, err = hostLimitInt("MaxConnections", o.MaxConnections, 0, 0); err != nil {
		return err
	}
	return nil
}

func (o *DeserializeOptions) normalizeLimits() (err error) {
	o.MaxStorageBytes, err = hostLimit("MaxStorageBytes", o.MaxStorageBytes, 0, 0)
	return err
}

// sessionLimits converts SessionOptions' limits to the internal sessionState forms.
func (o SessionOptions) sessionLimits() (SessionOptions, error) {
	var err error
	if o.MaxScalarBytes, err = hostLimit("MaxScalarBytes", o.MaxScalarBytes, defaultScalarBytes, math.MaxInt64); err != nil {
		return o, err
	}
	if o.MaxQueryMemoryBytes, err = hostLimit("MaxQueryMemoryBytes", o.MaxQueryMemoryBytes, 0, 0); err != nil {
		return o, err
	}
	if o.MaxCost, err = hostLimit("MaxCost", o.MaxCost, 0, 0); err != nil {
		return o, err
	}
	if o.LifetimeMaxCost, err = hostLimit("LifetimeMaxCost", o.LifetimeMaxCost, 0, 0); err != nil {
		return o, err
	}
	if o.MaxSQLLength, err = hostLimitInt("MaxSQLLength", o.MaxSQLLength, DefaultMaxSQLLength, 0); err != nil {
		return o, err
	}
	if o.WorkMem, err = hostLimitInt("WorkMem", o.WorkMem, defaultWorkMem, 0); err != nil {
		return o, err
	}
	if o.TempBuffers, err = hostLimitInt("TempBuffers", o.TempBuffers, DefaultTempBuffers, 0); err != nil {
		return o, err
	}
	if o.MaxTempStorageBytes, err = hostLimitInt("MaxTempStorageBytes", o.MaxTempStorageBytes, DefaultMaxTempStorageBytes, 0); err != nil {
		return o, err
	}
	return o, nil
}

// --- Session limits. Each getter reports the effective value (a default reads as its number, no
// limit as Unlimited); each setter follows the Unlimited rule. ---

// MaxCost returns the per-statement execution-cost limit, or Unlimited (the default).
func (s *Session) MaxCost() int64 { return reportLimit(s.maxCostRaw(), 0) }

// SetMaxCost sets the per-statement execution-cost limit. A statement that exceeds it fails with
// SQLSTATE 54P01. 0 restores the default, Unlimited.
func (s *Session) SetMaxCost(limit int64) error {
	n, err := hostLimit("MaxCost", limit, 0, 0)
	if err == nil {
		s.setMaxCostRaw(n)
	}
	return err
}

// LifetimeMaxCost returns the session's cumulative execution-cost budget, or Unlimited (the
// default).
func (s *Session) LifetimeMaxCost() int64 { return reportLimit(s.lifetimeMaxCostRaw(), 0) }

// SetLifetimeMaxCost sets the session's cumulative execution-cost budget. Once the session's
// total reaches it, the running statement fails with SQLSTATE 54P02, as does every later one.
// 0 restores the default, Unlimited.
func (s *Session) SetLifetimeMaxCost(limit int64) error {
	n, err := hostLimit("LifetimeMaxCost", limit, 0, 0)
	if err == nil {
		s.setLifetimeMaxCostRaw(n)
	}
	return err
}

// MaxSQLLength returns the longest SQL text, in bytes, the session accepts, or Unlimited.
func (s *Session) MaxSQLLength() int { return int(reportLimit(int64(s.maxSQLLengthRaw()), 0)) }

// SetMaxSQLLength sets the longest SQL text, in bytes, the session accepts; longer text fails with
// SQLSTATE 54000. 0 restores the default, DefaultMaxSQLLength.
func (s *Session) SetMaxSQLLength(bytes int) error {
	n, err := hostLimitInt("MaxSQLLength", bytes, DefaultMaxSQLLength, 0)
	if err == nil {
		s.setMaxSQLLengthRaw(n)
	}
	return err
}

// WorkMem returns the memory, in bytes, a sort or other blocking operator holds before it spills
// to scratch storage, or Unlimited (never spill).
func (s *Session) WorkMem() int { return int(reportLimit(int64(s.workMemRaw()), 0)) }

// SetWorkMem sets the memory, in bytes, a blocking operator holds before it spills to scratch
// storage. It never changes a query's results or cost. 0 restores the default (256 MiB);
// Unlimited never spills.
func (s *Session) SetWorkMem(bytes int) error {
	n, err := hostLimitInt("WorkMem", bytes, defaultWorkMem, 0)
	if err == nil {
		s.setWorkMemRaw(n)
	}
	return err
}

// TempBuffers returns the memory, in bytes, the session's temporary tables keep resident before
// they move to a scratch file, or Unlimited (never move).
func (s *Session) TempBuffers() int {
	return int(reportLimit(int64(s.engine.session.tempBuffers), 0))
}

// SetTempBuffers sets the memory, in bytes, the session's temporary tables keep resident before
// they move to a scratch file. It never changes results or cost. 0 restores the default,
// DefaultTempBuffers; Unlimited keeps them resident.
func (s *Session) SetTempBuffers(bytes int) error {
	n, err := hostLimitInt("TempBuffers", bytes, DefaultTempBuffers, 0)
	if err == nil {
		s.setTempBuffersRaw(n)
	}
	return err
}

// MaxTempStorageBytes returns the storage limit, in bytes, of the session's temporary tables, or
// Unlimited.
func (s *Session) MaxTempStorageBytes() int {
	return int(reportLimit(int64(s.engine.session.maxTempStorageBytes), 0))
}

// SetMaxTempStorageBytes sets the storage limit, in bytes, of the session's temporary tables,
// resident or moved to scratch storage alike. A write past it fails with SQLSTATE 54P03. 0
// restores the default, DefaultMaxTempStorageBytes.
func (s *Session) SetMaxTempStorageBytes(bytes int) error {
	n, err := hostLimitInt("MaxTempStorageBytes", bytes, DefaultMaxTempStorageBytes, 0)
	if err == nil {
		s.setMaxTempStorageBytesRaw(n)
	}
	return err
}

// MaxScalarBytes returns the per-statement budget, in bytes, for the values the statement
// computes, or Unlimited.
func (s *Session) MaxScalarBytes() int64 { return reportLimit(s.maxScalarBytesRaw(), math.MaxInt64) }

// SetMaxScalarBytes sets the per-statement budget, in bytes, for the values a statement computes;
// past it the statement fails with SQLSTATE 54P04. 0 restores the default (64 MiB).
func (s *Session) SetMaxScalarBytes(bytes int64) error {
	n, err := hostLimit("MaxScalarBytes", bytes, defaultScalarBytes, math.MaxInt64)
	if err == nil {
		s.setMaxScalarBytesRaw(n)
	}
	return err
}

// MaxQueryMemoryBytes returns the per-statement budget, in bytes, for the memory a query holds at
// once, or Unlimited (the default).
func (s *Session) MaxQueryMemoryBytes() int64 {
	if n := s.maxQueryMemoryBytesRaw(); n > 0 {
		return n
	}
	return Unlimited
}

// SetMaxQueryMemoryBytes sets the per-statement budget, in bytes, for the memory a query holds at
// once; past it the statement fails with SQLSTATE 54P05. An open Rows keeps the budget it started
// with. 0 restores the default, Unlimited.
func (s *Session) SetMaxQueryMemoryBytes(bytes int64) error {
	n, err := hostLimit("MaxQueryMemoryBytes", bytes, 0, 0)
	if err == nil {
		s.setMaxQueryMemoryBytesRaw(n)
	}
	return err
}

// SetMaxStorageBytes sets the storage limit, in bytes, of database name (main or an attachment).
// A commit that would grow the database past it fails with SQLSTATE 54P06. It applies to every
// session of the Database. 0 restores the default, Unlimited. A name that is not attached fails
// with SQLSTATE 42704.
func (db *Database) SetMaxStorageBytes(name string, bytes int64) error {
	n, err := hostLimit("MaxStorageBytes", bytes, 0, 0)
	if err != nil {
		return err
	}
	return db.setMaxStorageBytesRaw(name, n)
}
