package jed

// Host-defined scalar functions (spec/design/extensibility.md §4.2 / §5.1, delivery step 3). The
// registry/resolve/eval injection seam is a HOST-API surface the conformance corpus cannot express
// (it registers no host code), so it is tested per core (CLAUDE.md §10 — host-API is a sanctioned
// unit-test category). Mirrors impl/rust/tests/host_functions.rs one-for-one.

import (
	"path/filepath"
	"testing"
)

// hostAdd is host_add(i64, i64) -> i64 — integer sum (strict: never sees NULL).
func hostAdd() *HostFunction {
	return NewHostFunction("host_add", []string{"i64", "i64"}, "i64",
		func(args []Value) (Value, error) {
			return IntValue(args[0].Int + args[1].Int), nil
		}).WithVolatility(VolatilityImmutable).WithCrossCore(true)
}

// hostAddText is host_add(text, text) -> text — a same-name overload on a different signature.
func hostAddText() *HostFunction {
	return NewHostFunction("host_add", []string{"text", "text"}, "text",
		func(args []Value) (Value, error) {
			return TextValue(args[0].str() + args[1].str()), nil
		})
}

func regWith(t *testing.T, funcs ...*HostFunction) *ExtensionRegistry {
	t.Helper()
	r := NewExtensionRegistry()
	for _, f := range funcs {
		if err := r.RegisterFunction(f); err != nil {
			t.Fatalf("register: %v", err)
		}
	}
	return r
}

func dbExt(t *testing.T, ext *ExtensionRegistry, stmts ...string) *Session {
	t.Helper()
	db, err := CreateDatabase(CreateOptions{Extensions: ext})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	s := db.Session(SessionOptions{})
	for _, stmt := range stmts {
		if _, err := queryOutcome(s, stmt, nil); err != nil {
			t.Fatalf("setup %q: %v", stmt, err)
		}
	}
	return s
}

func hostRows(t *testing.T, s *Session, sql string) [][]Value {
	t.Helper()
	out, err := queryOutcome(s, sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	return out.Rows
}

func oneVal(t *testing.T, s *Session, sql string) Value {
	t.Helper()
	rows := hostRows(t, s, sql)
	if len(rows) != 1 || len(rows[0]) != 1 {
		t.Fatalf("%q: expected exactly one scalar row, got %d rows", sql, len(rows))
	}
	return rows[0][0]
}

func TestHostScalarFunctionOverLiterals(t *testing.T) {
	t.Parallel()
	s := dbExt(t, regWith(t, hostAdd()))
	if v := oneVal(t, s, "SELECT host_add(2, 3)"); v.Int != 5 {
		t.Fatalf("host_add(2,3) = %v, want 5", v.Int)
	}
	if v := oneVal(t, s, "SELECT host_add(host_add(1, 1), 40)"); v.Int != 42 {
		t.Fatalf("nested host_add = %v, want 42", v.Int)
	}
}

func TestHostScalarFunctionOverColumns(t *testing.T) {
	t.Parallel()
	s := dbExt(t, regWith(t, hostAdd()),
		"CREATE TABLE t (id i32 PRIMARY KEY, a i64, b i64)",
		"INSERT INTO t VALUES (1, 10, 20), (2, 100, 1)")
	rows := hostRows(t, s, "SELECT host_add(a, b) FROM t ORDER BY id")
	if len(rows) != 2 || rows[0][0].Int != 30 || rows[1][0].Int != 101 {
		t.Fatalf("host_add(a,b) = %v, want [30 101]", rows)
	}
}

func TestHostFunctionIsStrictOnTypedNull(t *testing.T) {
	t.Parallel()
	// A NULL-valued argument of a KNOWN type short-circuits to NULL before the kernel runs (§4.2);
	// the kernel (which would panic on a NULL Int) is never called.
	s := dbExt(t, regWith(t, hostAdd()),
		"CREATE TABLE t (id i32 PRIMARY KEY, a i64, b i64)",
		"INSERT INTO t VALUES (1, NULL, 20)")
	if v := oneVal(t, s, "SELECT host_add(a, b) FROM t"); !v.IsNull() {
		t.Fatalf("host_add(NULL,20) = %v, want NULL", v)
	}
}

func TestBareNullLiteralFindsNoOverload(t *testing.T) {
	t.Parallel()
	// A bare untyped NULL matches no concrete scalar signature — 42883, exactly as a built-in
	// (abs(NULL)) behaves. Strictness is an eval-time property of a TYPED null, not a resolution one.
	s := dbExt(t, regWith(t, hostAdd()))
	wantErr(t, s, "SELECT host_add(NULL, 3)", "42883")
}

func TestHostOverloadBySignature(t *testing.T) {
	t.Parallel()
	s := dbExt(t, regWith(t, hostAdd(), hostAddText()))
	if v := oneVal(t, s, "SELECT host_add(2, 3)"); v.Int != 5 {
		t.Fatalf("host_add(2,3) = %v, want 5", v.Int)
	}
	if v := oneVal(t, s, "SELECT host_add('foo', 'bar')"); v.str() != "foobar" {
		t.Fatalf("host_add('foo','bar') = %q, want foobar", v.str())
	}
}

func TestBuiltinWinsOverHostSameSignature(t *testing.T) {
	t.Parallel()
	// Registering a host abs(i64) is accepted but never reached — the built-in abs shadows it (§4.2).
	// If the host kernel (returning a sentinel 999) ran, abs(-5) would be 999.
	hostAbs := NewHostFunction("abs", []string{"i64"}, "i64",
		func(args []Value) (Value, error) { return IntValue(999), nil })
	s := dbExt(t, regWith(t, hostAbs))
	if v := oneVal(t, s, "SELECT abs(-5)"); v.Int != 5 {
		t.Fatalf("abs(-5) = %v, want 5 (built-in wins)", v.Int)
	}
}

func TestDuplicateSignatureRejected(t *testing.T) {
	t.Parallel()
	r := NewExtensionRegistry()
	if err := r.RegisterFunction(hostAdd()); err != nil {
		t.Fatalf("first register: %v", err)
	}
	// Same (name, arg types) — rejected 42723 (signature-level, §4.2).
	err := r.RegisterFunction(hostAdd())
	if ee, ok := err.(*EngineError); !ok || ee.Code() != "42723" {
		t.Fatalf("duplicate register = %v, want 42723", err)
	}
	// A different signature on the same name is fine (overloading).
	if err := r.RegisterFunction(hostAddText()); err != nil {
		t.Fatalf("overload register: %v", err)
	}
}

func TestNegativeCostRejected(t *testing.T) {
	t.Parallel()
	r := NewExtensionRegistry()
	bad := NewHostFunction("host_neg", nil, "i64",
		func(args []Value) (Value, error) { return IntValue(0), nil }).WithCost(-1)
	if ee, ok := r.RegisterFunction(bad).(*EngineError); !ok || ee.Code() != "22023" {
		t.Fatalf("negative cost register, want 22023")
	}
}

func TestUnknownTypeNameRejected(t *testing.T) {
	t.Parallel()
	r := NewExtensionRegistry()
	bad := NewHostFunction("host_bad", []string{"not_a_type"}, "i64",
		func(args []Value) (Value, error) { return IntValue(0), nil })
	if ee, ok := r.RegisterFunction(bad).(*EngineError); !ok || ee.Code() != "42704" {
		t.Fatalf("unknown type name register, want 42704")
	}
}

func TestDeclaredCostIsChargedPerCall(t *testing.T) {
	t.Parallel()
	// Two 0-arg functions identical but for their declared static weight; the query-cost difference
	// is exactly the weight difference (cost.md §6 design (a), charged once per call).
	const0 := func(name string, cost int64) *HostFunction {
		return NewHostFunction(name, nil, "i64",
			func(args []Value) (Value, error) { return IntValue(0), nil }).WithCost(cost)
	}
	s := dbExt(t, regWith(t, const0("host_c0", 0), const0("host_c1000", 1000)))
	out0, _ := queryOutcome(s, "SELECT host_c0()", nil)
	out1000, _ := queryOutcome(s, "SELECT host_c1000()", nil)
	if diff := out1000.Cost - out0.Cost; diff != 1000 {
		t.Fatalf("cost difference = %d, want 1000", diff)
	}
}

func TestDeclaredCostGatesMaxCostCeiling(t *testing.T) {
	t.Parallel()
	// A declared weight above the ceiling aborts 54P01 before the kernel runs (guard after charge).
	heavy := NewHostFunction("host_heavy", nil, "i64",
		func(args []Value) (Value, error) { return IntValue(0), nil }).WithCost(1_000_000)
	db, _ := CreateDatabase(CreateOptions{Extensions: regWith(t, heavy)})
	s := db.Session(SessionOptions{})
	s.SetMaxCost(1000)
	_, err := queryOutcome(s, "SELECT host_heavy()", nil)
	if ee, ok := err.(*EngineError); !ok || ee.Code() != "54P01" {
		t.Fatalf("over-ceiling host call = %v, want 54P01", err)
	}
}

func TestWrongResultTypeIsRejected(t *testing.T) {
	t.Parallel()
	// A kernel that violates its declared RETURNS i64 (returns text) is caught (22000) rather than
	// leaking a wrong-typed value into jed's strict type system (CLAUDE.md §13).
	liar := NewHostFunction("host_liar", nil, "i64",
		func(args []Value) (Value, error) { return TextValue("oops"), nil })
	s := dbExt(t, regWith(t, liar))
	wantErr(t, s, "SELECT host_liar()", "22000")
}

func TestUnknownHostFunctionStillUndefined(t *testing.T) {
	t.Parallel()
	s := dbExt(t, regWith(t, hostAdd()))
	wantErr(t, s, "SELECT host_missing(1)", "42883")
}

func TestExplainRendersHostFunctionName(t *testing.T) {
	t.Parallel()
	s := dbExt(t, regWith(t, hostAdd()), "CREATE TABLE t (id i32 PRIMARY KEY, a i64, b i64)")
	rows := hostRows(t, s, "EXPLAIN (VERBOSE) SELECT host_add(a, b) FROM t")
	found := false
	for _, r := range rows {
		for _, v := range r {
			if v.Kind == ValText && contains(v.str(), "host_add(") {
				found = true
			}
		}
	}
	if !found {
		t.Fatalf("EXPLAIN VERBOSE should render the host function name")
	}
}

func TestNoExtensionsIsUnaffected(t *testing.T) {
	t.Parallel()
	// The built-in-only path is untouched: an empty registry resolves nothing new, and a call to a
	// would-be host name is 42883.
	s := dbExt(t, NewExtensionRegistry())
	if v := oneVal(t, s, "SELECT abs(-7)"); v.Int != 7 {
		t.Fatalf("abs(-7) = %v, want 7", v.Int)
	}
	wantErr(t, s, "SELECT host_add(1, 2)", "42883")
	// A nil registry (no Extensions option) behaves the same.
	s2 := memDB().Session(SessionOptions{})
	wantErr(t, s2, "SELECT host_add(1, 2)", "42883")
}

func contains(haystack, needle string) bool {
	for i := 0; i+len(needle) <= len(haystack); i++ {
		if haystack[i:i+len(needle)] == needle {
			return true
		}
	}
	return false
}

// ---------------------------------------------------------------------------------------------
// Delivery step 4 (extensibility.md §8.1 / §14): host scalar functions in PERSISTED INDEXES. An
// `immutable` host function carrying a component_id + semantic_version may back an expression /
// partial index; the file records the resolved dependency (format_version 31) and re-checks it on
// reopen. A missing / different-component / bumped-version function makes the index unusable — skipped
// for reads (correct heap scan), refused for writes (read-only) — never a silent stale-key read. These
// cover what the corpus cannot express (host-API registration + on-disk reopen with a *different*
// registry); they mirror the Rust/TS host-function tests one-for-one.
// ---------------------------------------------------------------------------------------------

// geoHash is geo_hash(i64) -> i64 — the canonical index-backing host function. component/version pin
// its identity; Immutable + a component id are the two admission requirements (§8.1).
func geoHash(component string, version uint32) *HostFunction {
	return NewHostFunction("geo_hash", []string{"i64"}, "i64",
		func(args []Value) (Value, error) { return IntValue(args[0].Int * 10), nil }).
		WithVolatility(VolatilityImmutable).WithCrossCore(true).
		WithComponentID(component).WithSemanticVersion(version)
}

// geoIDs runs sql and returns its first column as int64s (the reopen tests read `id`).
func geoIDs(t *testing.T, q valueQuerier, sql string) []int64 {
	t.Helper()
	out, err := queryOutcome(q, sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	ids := make([]int64, 0, len(out.Rows))
	for _, r := range out.Rows {
		ids = append(ids, r[0].Int)
	}
	return ids
}

// createGeoFile creates a file-backed database with reg registered and runs the setup statements
// (each autocommits durably), then closes it.
func createGeoFile(t *testing.T, path string, reg *ExtensionRegistry, stmts ...string) {
	t.Helper()
	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true, Extensions: reg})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	defer db.Close()
	for _, s := range stmts {
		if _, err := queryOutcome(db, s, nil); err != nil {
			t.Fatalf("setup %q: %v", s, err)
		}
	}
}

// openGeoFile reopens a file-backed database (v31 deserialize) with reg registered.
func openGeoFile(t *testing.T, path string, reg *ExtensionRegistry) *Database {
	t.Helper()
	db, err := OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true, Extensions: reg})
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	return db
}

func TestHostFuncVolatileInIndexRejected(t *testing.T) {
	t.Parallel()
	// The latent-bug fix: a volatile host function used to leak silently into an index expression (the
	// immutability gate was purely syntactic and did not see host functions). Now 42P17.
	volatile := NewHostFunction("geo_hash", []string{"i64"}, "i64",
		func(args []Value) (Value, error) { return IntValue(args[0].Int * 10), nil }).
		WithComponentID("com.example/geo_hash") // Volatile by default
	s := dbExt(t, regWith(t, volatile), "CREATE TABLE t (a i64)")
	wantErr(t, s, "CREATE INDEX ix ON t (geo_hash(a))", "42P17")
}

func TestHostFuncUnversionedInIndexRejected(t *testing.T) {
	t.Parallel()
	// Immutable but no component identity → cannot persist a sound dependency (42P17).
	unversioned := NewHostFunction("geo_hash", []string{"i64"}, "i64",
		func(args []Value) (Value, error) { return IntValue(args[0].Int * 10), nil }).
		WithVolatility(VolatilityImmutable)
	s := dbExt(t, regWith(t, unversioned), "CREATE TABLE t (a i64)")
	wantErr(t, s, "CREATE INDEX ix ON t (geo_hash(a))", "42P17")
}

func TestHostFuncImmutableVersionedInIndexOK(t *testing.T) {
	t.Parallel()
	s := dbExt(t, regWith(t, geoHash("com.example/geo_hash", 1)),
		"CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
		"INSERT INTO t VALUES (1, 3), (2, 7)",
		"CREATE INDEX ix ON t (geo_hash(a))")
	// geo_hash(3) = 30 → row id 1.
	if got := geoIDs(t, s, "SELECT id FROM t WHERE geo_hash(a) = 30"); len(got) != 1 || got[0] != 1 {
		t.Fatalf("SELECT id WHERE geo_hash(a)=30 = %v, want [1]", got)
	}
}

func TestHostFuncIndexReopenMatchingOK(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "hostfunc_index_match.jed")
	createGeoFile(t, path, regWith(t, geoHash("com.example/geo_hash", 1)),
		"CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
		"INSERT INTO t VALUES (1, 3), (2, 7)",
		"CREATE INDEX ix ON t (geo_hash(a))")
	// Reopen (v31 deserialize) with the SAME component + version: the dependency matches, so reads use
	// the index and writes maintain it.
	db := openGeoFile(t, path, regWith(t, geoHash("com.example/geo_hash", 1)))
	defer db.Close()
	if got := geoIDs(t, db, "SELECT id FROM t WHERE geo_hash(a) = 30"); len(got) != 1 || got[0] != 1 {
		t.Fatalf("matching reopen read = %v, want [1]", got)
	}
	if _, err := queryOutcome(db, "INSERT INTO t VALUES (3, 3)", nil); err != nil {
		t.Fatalf("a write maintaining a matching host-dep index should succeed: %v", err)
	}
	if got := geoIDs(t, db, "SELECT id FROM t WHERE geo_hash(a) = 30 ORDER BY id"); len(got) != 2 || got[0] != 1 || got[1] != 3 {
		t.Fatalf("after insert = %v, want [1 3]", got)
	}
}

func TestHostFuncIndexReopenVersionBumpUnusable(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "hostfunc_index_bump.jed")
	createGeoFile(t, path, regWith(t, geoHash("com.example/geo_hash", 1)),
		"CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
		"INSERT INTO t VALUES (1, 3), (2, 7)",
		"CREATE INDEX ix ON t (geo_hash(a))")
	// Reopen with a BUMPED semantic_version → the index's stored keys are stale.
	db := openGeoFile(t, path, regWith(t, geoHash("com.example/geo_hash", 2)))
	defer db.Close()
	// Reads still correct: a plain read (no index) and one that COULD use the index (skipped → heap
	// scan) both return the right rows — never a silent stale-key read.
	if got := geoIDs(t, db, "SELECT id FROM t ORDER BY id"); len(got) != 2 || got[0] != 1 || got[1] != 2 {
		t.Fatalf("plain read = %v, want [1 2]", got)
	}
	if got := geoIDs(t, db, "SELECT id FROM t WHERE geo_hash(a) = 30"); len(got) != 1 || got[0] != 1 {
		t.Fatalf("index-skipped read = %v, want [1]", got)
	}
	// A write that would maintain the stale index is refused (XX002) — the table is read-only.
	if _, err := queryOutcome(db, "INSERT INTO t VALUES (3, 3)", nil); errCodeOf(err) != "XX002" {
		t.Fatalf("write over a version-bumped host-dep index = %v, want XX002", err)
	}
}

func TestHostFuncIndexReopenDifferentComponentUnusable(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "hostfunc_index_component.jed")
	createGeoFile(t, path, regWith(t, geoHash("com.example/geo_hash", 1)),
		"CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
		"INSERT INTO t VALUES (1, 3)",
		"CREATE INDEX ix ON t (geo_hash(a))")
	// Reopen with a DIFFERENT component id for the same name/signature → a different implementation.
	db := openGeoFile(t, path, regWith(t, geoHash("org.other/geo_hash", 1)))
	defer db.Close()
	if got := geoIDs(t, db, "SELECT id FROM t WHERE geo_hash(a) = 30"); len(got) != 1 || got[0] != 1 {
		t.Fatalf("different-component read = %v, want [1]", got)
	}
	if _, err := queryOutcome(db, "INSERT INTO t VALUES (2, 3)", nil); errCodeOf(err) != "XX002" {
		t.Fatalf("write over a different-component host-dep index = %v, want XX002", err)
	}
}

func TestHostFuncIndexReopenMissingFunction(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "hostfunc_index_missing.jed")
	createGeoFile(t, path, regWith(t, geoHash("com.example/geo_hash", 1)),
		"CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
		"INSERT INTO t VALUES (1, 3), (2, 7)",
		"CREATE INDEX ix ON t (geo_hash(a))")
	// Reopen with NO extensions: the index expression can no longer resolve.
	db := openGeoFile(t, path, NewExtensionRegistry())
	defer db.Close()
	// A read that does not reference the missing function still works (the index is simply unused).
	if got := geoIDs(t, db, "SELECT id FROM t ORDER BY id"); len(got) != 2 || got[0] != 1 || got[1] != 2 {
		t.Fatalf("plain read with missing function = %v, want [1 2]", got)
	}
	// A write that would maintain the index needs the missing function → 42883 (resolution fails).
	if _, err := queryOutcome(db, "INSERT INTO t VALUES (3, 3)", nil); errCodeOf(err) != "42883" {
		t.Fatalf("write needing the missing function = %v, want 42883", err)
	}
}
