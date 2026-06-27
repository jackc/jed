package main

// Regression guards for the Layer 3 stress runner (spec/design/concurrency-testing.md §6). The
// bench module is not in `rake ci` (bench-family), so these run via `go test ./cmd/stress` during
// bench work — the same convention as prng_test.go / checksum_test.go. They lock in the two
// properties the cross-core `rake stress` check depends on: the seeded interleaver is deterministic,
// and a confluent workload's final checksum is mode-independent (sequential == threaded).

import (
	jed "github.com/jackc/jed/impl/go"
	"testing"
)

// balanceFile is a small balance-transfer workload (the §6 shape, tiny counts for a fast test).
func balanceFile() *stressFile {
	return &stressFile{
		Meta: stressMeta{Name: "t", Parallel: "optional", Seed: 1234},
		Setup: stressSetup{SQL: []string{
			"CREATE TABLE acct (id i32 PRIMARY KEY, bal i64)",
			"INSERT INTO acct VALUES (1, 100), (2, 0)",
		}},
		Worker: []stressWorker{
			{Kind: "writer", Count: 2, Iterations: 50, Op: "BEGIN; UPDATE acct SET bal = bal - 1 WHERE id = 1; UPDATE acct SET bal = bal + 1 WHERE id = 2; COMMIT;"},
			{Kind: "reader", Count: 3, Iterations: 40, InvariantQuery: "SELECT sum(bal) FROM acct", InvariantExpect: "100"},
		},
		Final: &stressFinal{Query: "SELECT id, bal FROM acct ORDER BY id", Expect: [][]int64{{1, 0}, {2, 100}}, CrossCoreChecksum: true},
	}
}

// runOnce sets up a fresh handle, runs the workload one way, and returns the final checksum.
func runOnce(t *testing.T, f *stressFile, sequential bool) string {
	t.Helper()
	db := jed.NewSharedDB()
	if err := setup(db, f); err != nil {
		t.Fatalf("setup: %v", err)
	}
	var err error
	if sequential {
		_, err = runSequential(db, f)
	} else {
		_, err = runThreaded(db, f)
	}
	if err != nil {
		t.Fatalf("run (sequential=%v): %v", sequential, err)
	}
	sum, ok, ferr := checkFinal(db, f.Final)
	if ferr != nil {
		t.Fatalf("checkFinal: %v", ferr)
	}
	if !ok {
		t.Fatalf("final state mismatch (sequential=%v)", sequential)
	}
	return sum
}

// TestSequentialDeterministic: the seeded interleaver is a pure function of the seed.
func TestSequentialDeterministic(t *testing.T) {
	f := balanceFile()
	a := runOnce(t, f, true)
	b := runOnce(t, balanceFile(), true)
	if a != b {
		t.Fatalf("seeded-sequential not deterministic: %s != %s", a, b)
	}
}

// TestModeAgreement: a confluent workload's final checksum is the same under the seeded interleaver
// and under real goroutines — the property `rake stress` cross-checks across cores.
func TestModeAgreement(t *testing.T) {
	seq := runOnce(t, balanceFile(), true)
	threaded := runOnce(t, balanceFile(), false)
	if seq != threaded {
		t.Fatalf("sequential checksum %s != threaded checksum %s", seq, threaded)
	}
}

// TestInvariantCatchesBug: the per-snapshot invariant is non-vacuous — a wrong expectation fails.
func TestInvariantCatchesBug(t *testing.T) {
	f := balanceFile()
	f.Worker[1].InvariantExpect = "999" // the true sum is always 100
	db := jed.NewSharedDB()
	if err := setup(db, f); err != nil {
		t.Fatalf("setup: %v", err)
	}
	if _, err := runSequential(db, f); err == nil {
		t.Fatal("expected the invariant check to fail, but it passed")
	}
}
