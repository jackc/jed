package jed

// uuid ⇄ bytea casts — the deliberate PostgreSQL divergences (spec/types/casts.toml,
// spec/design/types.md §14). PostgreSQL has NO bytea↔uuid cast (bytea::uuid / uuid::bytea is 42846
// cannot_coerce); jed adds both as EXPLICIT casts over the 16 raw bytes, so they SUCCEED where PG
// errors and cannot live in the PG-clean oracle corpus. The text↔uuid casts (which AGREE with PG)
// are oracle-checked in suites/cast/uuid.test and run on every core; a couple of smoke checks here
// run alongside (CLAUDE.md §10). Mirrors impl/rust/tests/cast_uuid.rs.

import "testing"

// the 16 raw bytes of 550e8400-e29b-41d4-a716-446655440000.
var uuid16 = string([]byte{
	0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44, 0x00, 0x00,
})

// uuid → bytea is the 16 raw bytes (PG: 42846 — jed adds this cast).
func TestUuidToByteaIsThe16Bytes(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	v := castOne(t, db, "SELECT '550e8400-e29b-41d4-a716-446655440000'::uuid::bytea")
	if v.Kind != ValBytea || v.str() != uuid16 {
		t.Fatalf("uuid::bytea = %v, want the 16 bytes", v)
	}
}

// bytea → uuid takes the 16 raw bytes (PG: 42846 — jed adds this cast).
func TestByteaToUuidIsThe16Bytes(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	v := castOne(t, db, "SELECT '\\x550e8400e29b41d4a716446655440000'::bytea::uuid")
	if v.Kind != ValUuid || v.str() != uuid16 {
		t.Fatalf("bytea::uuid = %v, want the 16 bytes", v)
	}
}

// bytea → uuid requires EXACTLY 16 bytes; any other length traps 22P02 (the wrong-width body —
// there is no PG code to match, so jed reuses invalid_text_representation).
func TestByteaToUuidWrongLengthTraps22P02(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	for _, sql := range []string{
		"SELECT '\\xabcd'::bytea::uuid",                               // 2 bytes
		"SELECT '\\x'::bytea::uuid",                                   // empty (0 bytes)
		"SELECT '\\x550e8400e29b41d4a71644665544000000'::bytea::uuid", // 17 bytes
	} {
		if code := castErrCode(t, db, sql); code != "22P02" {
			t.Fatalf("%q: got %s, want 22P02", sql, code)
		}
	}
}

// The casts round-trip through real columns (the runtime, non-constant path); NULL adapts.
func TestUuidByteaRoundTripThroughColumns(t *testing.T) {
	t.Parallel()
	db := dbWith(
		t,
		"CREATE TABLE t (id i32 PRIMARY KEY, u uuid, b bytea)",
		"INSERT INTO t VALUES (1, '550e8400-e29b-41d4-a716-446655440000', "+
			"'\\x550e8400e29b41d4a716446655440000'), (2, NULL, NULL)",
	)
	if v := castOne(t, db, "SELECT u::bytea FROM t WHERE id = 1"); v.Kind != ValBytea || v.str() != uuid16 {
		t.Fatalf("u::bytea = %v, want the 16 bytes", v)
	}
	if v := castOne(t, db, "SELECT b::uuid FROM t WHERE id = 1"); v.Kind != ValUuid || v.str() != uuid16 {
		t.Fatalf("b::uuid = %v, want the 16 bytes", v)
	}
	if v := castOne(t, db, "SELECT u::bytea FROM t WHERE id = 2"); v.Kind != ValNull {
		t.Fatalf("NULL u::bytea = %v, want NULL", v)
	}
	if v := castOne(t, db, "SELECT b::uuid FROM t WHERE id = 2"); v.Kind != ValNull {
		t.Fatalf("NULL b::uuid = %v, want NULL", v)
	}
}

// text → uuid / uuid → text smoke check (the oracle-corpus behavior, run here per core too).
func TestTextUuidSmoke(t *testing.T) {
	t.Parallel()
	db := dbWith(
		t,
		"CREATE TABLE t (id i32 PRIMARY KEY, s text, u uuid)",
		"INSERT INTO t VALUES (1, '550E8400-E29B-41D4-A716-446655440000', "+
			"'550e8400-e29b-41d4-a716-446655440000')",
		"INSERT INTO t VALUES (2, 'not-a-uuid', NULL)",
	)
	// an UPPERCASE text value casts to the same 16 bytes (renders lowercase)
	if v := castOne(t, db, "SELECT s::uuid FROM t WHERE id = 1"); v.Kind != ValUuid || v.str() != uuid16 {
		t.Fatalf("s::uuid = %v, want the 16 bytes", v)
	}
	if v := castOne(t, db, "SELECT u::text FROM t WHERE id = 1"); v.Kind != ValText ||
		v.str() != "550e8400-e29b-41d4-a716-446655440000" {
		t.Fatalf("u::text = %v, want canonical lowercase", v)
	}
	// a malformed runtime text → uuid traps 22P02 (the column path, not a literal)
	if code := castErrCode(t, db, "SELECT s::uuid FROM t WHERE id = 2"); code != "22P02" {
		t.Fatalf("malformed s::uuid: got %s, want 22P02", code)
	}
}
