package jed

import "testing"

func TestHashJoinRejectsForcedHashCollisions(t *testing.T) {
	plan := &hashJoinPlan{keys: []hashJoinKey{{
		left:  0,
		right: 1,
		ty:    dataType{Scalar: scalarInt32},
	}}}
	right := []storedRow{
		{IntValue(1), IntValue(10)},
		{IntValue(2), IntValue(20)},
		{IntValue(2), IntValue(21)},
	}
	table, err := newHashJoinTableWithHash(plan, 1, 0, right, newMeter(), func([]byte) uint64 { return 0 })
	if err != nil {
		t.Fatal(err)
	}
	got, err := table.probe(plan, storedRow{IntValue(2)}, newMeter())
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 2 || got[0][1].Int != 20 || got[1][1].Int != 21 {
		t.Fatalf("collision probe returned %#v", got)
	}
}
