package jed

import (
	"context"
	"testing"
)

// The Go host API's one limit rule (limits.go): positive is the limit, 0 the default, Unlimited no
// limit, any other negative 22023. Host-API surface only — the corpus sets limits through directives.

func TestSessionLimitRule(t *testing.T) {
	db := mustDB(t)
	s, err := db.Session(SessionOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	type limit struct {
		name     string
		set      func(int64) error
		get      func() int64
		def, unl int64
	}
	limits := []limit{
		{"MaxCost", s.SetMaxCost, s.MaxCost, Unlimited, 0},
		{"LifetimeMaxCost", s.SetLifetimeMaxCost, s.LifetimeMaxCost, Unlimited, 0},
		{"MaxSQLLength", func(n int64) error { return s.SetMaxSQLLength(int(n)) }, func() int64 { return int64(s.MaxSQLLength()) }, DefaultMaxSQLLength, 0},
		{"WorkMem", func(n int64) error { return s.SetWorkMem(int(n)) }, func() int64 { return int64(s.WorkMem()) }, defaultWorkMem, 0},
		{"TempBuffers", func(n int64) error { return s.SetTempBuffers(int(n)) }, func() int64 { return int64(s.TempBuffers()) }, DefaultTempBuffers, 0},
		{"MaxTempStorageBytes", func(n int64) error { return s.SetMaxTempStorageBytes(int(n)) }, func() int64 { return int64(s.MaxTempStorageBytes()) }, DefaultMaxTempStorageBytes, 0},
		{"MaxScalarBytes", s.SetMaxScalarBytes, s.MaxScalarBytes, defaultScalarBytes, 0},
		{"MaxQueryMemoryBytes", s.SetMaxQueryMemoryBytes, s.MaxQueryMemoryBytes, Unlimited, 0},
	}
	for _, l := range limits {
		if got := l.get(); got != l.def {
			t.Errorf("%s default = %d, want %d", l.name, got, l.def)
		}
		if err := l.set(4096); err != nil || l.get() != 4096 {
			t.Errorf("%s(4096): %v, reads %d", l.name, err, l.get())
		}
		if err := l.set(Unlimited); err != nil || l.get() != Unlimited {
			t.Errorf("%s(Unlimited): %v, reads %d", l.name, err, l.get())
		}
		if err := l.set(0); err != nil || l.get() != l.def {
			t.Errorf("%s(0): %v, reads %d, want the default %d", l.name, err, l.get(), l.def)
		}
		wantState(t, l.set(-2), InvalidParameterValue)
		if got := l.get(); got != l.def {
			t.Errorf("%s changed by a rejected value: %d", l.name, got)
		}
	}
}

func TestSessionOptionsLimitRule(t *testing.T) {
	db := mustDB(t)
	s, err := db.Session(SessionOptions{MaxSQLLength: Unlimited, WorkMem: 1 << 20, MaxCost: 0})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	if s.MaxSQLLength() != Unlimited || s.WorkMem() != 1<<20 || s.MaxCost() != Unlimited {
		t.Fatalf("got MaxSQLLength %d, WorkMem %d, MaxCost %d", s.MaxSQLLength(), s.WorkMem(), s.MaxCost())
	}
	_, err = db.Session(SessionOptions{TempBuffers: -5})
	wantState(t, err, InvalidParameterValue)
}

func TestStorageAndOpenLimitRule(t *testing.T) {
	db := mustDB(t)
	if err := db.SetMaxStorageBytes("main", Unlimited); err != nil {
		t.Fatal(err)
	}
	wantState(t, db.SetMaxStorageBytes("main", -2), InvalidParameterValue)
	_, err := Create(CreateOptions{MaxStorageBytes: -3})
	wantState(t, err, InvalidParameterValue)
	_, err = Open(t.TempDir()+"/missing.jed", OpenOptions{CacheBytes: -3})
	wantState(t, err, InvalidParameterValue)
	_, err = db.IntegrityCheck(context.Background(), "main", IntegrityCheckOptions{MaxFindings: -2})
	wantState(t, err, InvalidParameterValue)
	if _, err := db.IntegrityCheck(context.Background(), "main", IntegrityCheckOptions{MaxFindings: Unlimited}); err != nil {
		t.Fatal(err)
	}
}

func mustDB(t *testing.T) *Database {
	t.Helper()
	db, err := Create(CreateOptions{})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = db.Close() })
	return db
}
