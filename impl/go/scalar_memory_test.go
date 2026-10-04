package jed

import (
	"math"
	"testing"
)

// SQL admission/rollback/cost lives in the shared corpus. This pins host-only cursor
// ownership and budget changes while an older cursor is still alive.
func TestScalarBudgetCursorOwnership(t *testing.T) {
	s := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1), (2)")
	if s.MaxScalarBytes() != defaultScalarBytes {
		t.Fatal("missing finite default")
	}
	s.SetMaxScalarBytes(3)
	old, err := s.queryValues("SELECT repeat('x', 3) FROM t ORDER BY id", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer old.Close()
	if !old.Next() {
		t.Fatal(old.Err())
	}
	// Another statement owns a fresh account, and changing the limit does not alter old.
	s.SetMaxScalarBytes(100)
	if _, err := queryOutcome(s, "SELECT repeat('y', 9)", nil); err != nil {
		t.Fatal(err)
	}
	if old.Next() {
		t.Fatal("cursor's allocation account was reset")
	}
	if e, ok := old.Err().(*EngineError); !ok || e.Code() != "54P04" {
		t.Fatalf("wanted 54P04: %v", old.Err())
	}
	s.SetMaxScalarBytes(0)
	if s.MaxScalarBytes() != defaultScalarBytes {
		t.Fatal("zero disabled the budget")
	}
}

func TestResourceCounterSaturation(t *testing.T) {
	total := int64(math.MaxInt64 - 2)
	m := &costMeter{Limit: math.MaxInt64, lifetimeTotal: &total, lifetimeLimit: math.MaxInt64 - 1}
	m.Charge(math.MaxInt64)
	if m.Accrued != math.MaxInt64 || total != math.MaxInt64 {
		t.Fatal("cost wrapped")
	}
	if e, ok := m.Guard().(*EngineError); !ok || e.Code() != "54P02" {
		t.Fatal("lost first-crossed ceiling")
	}
	m.Charge(10)
	if m.Accrued != math.MaxInt64 {
		t.Fatal("cost wrapped after saturation")
	}
}
