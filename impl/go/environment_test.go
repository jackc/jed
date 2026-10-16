package jed

// Environment host-API tests (spec/design/environment.md §9): isolation (two environments in one
// process see different data and clocks), derivation (ToBuilder shares contents and changes only what
// it is told to), and lifetime (a database outlives the caller's reference to its environment). The
// corpus runs every record against one environment, so these per-environment behaviors are out of its
// reach (CLAUDE.md §10). Mirrors the Rust and TS cores' environment tests.

import (
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"testing"
)

var (
	testEnvMu    sync.Mutex
	testEnvCache = map[string]*Environment{}
)

// testEnv returns an Environment with the named spec fixture bundles loaded, in order — "unicode"
// (spec/collation/fixtures/unicode.jucd) and/or "tz" (spec/tz/fixtures/tzdata.jtz). Built once per
// bundle list and shared: an Environment is immutable, so sharing cannot leak state between tests.
// Panics if a fixture is missing or malformed (a broken checkout, not a test outcome).
func testEnv(bundles ...string) *Environment {
	key := strings.Join(bundles, "+")
	testEnvMu.Lock()
	defer testEnvMu.Unlock()
	if env := testEnvCache[key]; env != nil {
		return env
	}
	b := NewEnvironmentBuilder()
	for _, name := range bundles {
		var err error
		switch name {
		case "unicode":
			err = b.LoadUnicodeData(readSpecFixture("collation/fixtures/unicode.jucd"))
		case "tz":
			err = b.LoadTimeZoneData(readSpecFixture("tz/fixtures/tzdata.jtz"))
		default:
			panic("unknown test bundle " + name)
		}
		if err != nil {
			panic("load " + name + " bundle: " + err.Error())
		}
	}
	env := b.Build()
	testEnvCache[key] = env
	return env
}

// readSpecFixture reads spec/<rel>, searching up from the working directory (specPath without a t).
func readSpecFixture(rel string) []byte {
	dir, _ := os.Getwd()
	for {
		data, err := os.ReadFile(filepath.Join(dir, "spec", rel))
		if err == nil {
			return data
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			panic("could not locate spec/" + rel)
		}
		dir = parent
	}
}

func TestEnvironmentsAreIsolated(t *testing.T) {
	t.Parallel()
	// Two environments in one process: one with the Unicode + tz bundles and a manual clock at one
	// instant, one empty with a manual clock at another. Each database sees only its own.
	loadedBuilder := testEnv("unicode", "tz").ToBuilder()
	loadedBuilder.SetClockSource(NewManualClock(1_000_000))
	loaded := memDBIn(loadedBuilder.Build()).Session(SessionOptions{})
	emptyBuilder := NewEnvironmentBuilder()
	emptyBuilder.SetClockSource(NewManualClock(2_000_000))
	empty := memDBIn(emptyBuilder.Build()).Session(SessionOptions{})

	if rows := query(t, loaded, `SELECT 'ä' < 'z' COLLATE "unicode"`); len(rows) != 1 || rows[0][0] != BoolValue(true) {
		t.Fatalf("loaded env collation: got %v", rows)
	}
	if _, err := queryOutcome(empty, `SELECT 'ä' < 'z' COLLATE "unicode"`, nil); err == nil || err.(*EngineError).Code() != "42704" {
		t.Fatalf("empty env collation: want 42704, got %v", err)
	}
	if _, err := queryOutcome(loaded, `SELECT now() AT TIME ZONE 'America/New_York'`, nil); err != nil {
		t.Fatalf("loaded env zone: %v", err)
	}
	if _, err := queryOutcome(empty, `SELECT now() AT TIME ZONE 'America/New_York'`, nil); err == nil || err.(*EngineError).Code() != "22023" {
		t.Fatalf("empty env zone: want 22023, got %v", err)
	}
	if got := query(t, loaded, `SELECT now()`)[0][0]; got != TimestamptzValue(1_000_000) {
		t.Fatalf("loaded env clock: got %v", got)
	}
	if got := query(t, empty, `SELECT now()`)[0][0]; got != TimestamptzValue(2_000_000) {
		t.Fatalf("empty env clock: got %v", got)
	}
	if n := len(empty.LoadedCollations()); n != 0 {
		t.Fatalf("empty env lists %d collations", n)
	}
	if n := len(loaded.LoadedTimeZones()); n == 0 {
		t.Fatal("loaded env lists no zones")
	}
	// No environment given ⇒ a fresh empty one, never a process-wide default.
	if memDB().Environment() == memDB().Environment() {
		t.Fatal("two databases created with no environment must not share one")
	}
}

func TestEnvironmentDerivation(t *testing.T) {
	t.Parallel()
	// ToBuilder shares the base's contents and changes only what it is told to; the base is unchanged.
	baseBuilder := NewEnvironmentBuilder()
	if err := baseBuilder.LoadUnicodeData(readSpecFixture("collation/fixtures/unicode.jucd")); err != nil {
		t.Fatal(err)
	}
	if err := baseBuilder.RegisterFunction(NewHostFunction("plus_one", []string{"i64"}, "i64", func(a []Value) (Value, error) {
		return IntValue(a[0].Int + 1), nil
	})); err != nil {
		t.Fatal(err)
	}
	baseClock := NewManualClock(10)
	baseBuilder.SetClockSource(baseClock)
	base := baseBuilder.Build()

	derivedBuilder := base.ToBuilder()
	derivedClock := NewManualClock(20)
	derivedBuilder.SetClockSource(derivedClock)
	if err := derivedBuilder.LoadTimeZoneData(readSpecFixture("tz/fixtures/tzdata.jtz")); err != nil {
		t.Fatal(err)
	}
	// A registration conflicting with an inherited one is still 42723.
	if err := derivedBuilder.RegisterFunction(NewHostFunction("plus_one", []string{"i64"}, "i64", nil)); err == nil || err.(*EngineError).Code() != "42723" {
		t.Fatalf("conflicting derived registration: want 42723, got %v", err)
	}
	derived := derivedBuilder.Build()

	if derived.collation("unicode") != base.collation("unicode") {
		t.Fatal("derived environment must share the base's collation tables by reference")
	}
	if derived.clock != derivedClock || base.clock != baseClock {
		t.Fatal("only the derived clock may change")
	}
	if derived.random != base.random {
		t.Fatal("the random source was not replaced, so it must be shared")
	}
	if len(base.LoadedTimeZones()) != 0 || len(derived.LoadedTimeZones()) == 0 {
		t.Fatal("tz data loaded into the derived builder must not reach the base")
	}
	db := memDBIn(derived).Session(SessionOptions{})
	if got := query(t, db, `SELECT plus_one(41), now()`)[0]; got[0] != IntValue(42) || got[1] != TimestamptzValue(20) {
		t.Fatalf("derived environment: got %v", got)
	}
	// The builder keeps working after Build without touching what it built.
	if err := baseBuilder.LoadTimeZoneData(readSpecFixture("tz/fixtures/tzdata.jtz")); err != nil {
		t.Fatal(err)
	}
	if len(base.LoadedTimeZones()) != 0 {
		t.Fatal("a later builder load must not change an already-built environment")
	}
}

func TestDatabaseOutlivesEnvironmentReference(t *testing.T) {
	t.Parallel()
	// The handle keeps its environment alive: after the caller drops every reference and a GC runs,
	// the database still resolves the environment's collation and reads its clock.
	db := func() *Session {
		b := testEnv("unicode").ToBuilder()
		b.SetClockSource(NewManualClock(7))
		return memDBIn(b.Build()).Session(SessionOptions{})
	}()
	runtime.GC()
	if rows := query(t, db, `SELECT 'ä' < 'z' COLLATE "unicode", now()`); rows[0][0] != BoolValue(true) || rows[0][1] != TimestamptzValue(7) {
		t.Fatalf("after dropping the environment reference: got %v", rows)
	}
}
