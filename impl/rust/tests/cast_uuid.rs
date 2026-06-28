//! uuid ⇄ bytea casts — the deliberate PostgreSQL divergences (spec/types/casts.toml,
//! spec/design/types.md §14). PostgreSQL has NO bytea↔uuid cast (`bytea::uuid` / `uuid::bytea` is
//! 42846 cannot_coerce); jed adds both as EXPLICIT casts over the 16 raw bytes, so they SUCCEED
//! where PG errors and cannot live in the PG-clean oracle corpus. The text↔uuid casts (which AGREE
//! with PG) are oracle-checked in `suites/cast/uuid.test` and run on every core; a couple of smoke
//! checks here run alongside (CLAUDE.md §10 — the per-core test covers only what the corpus cannot).

use jed::value::Value;
use jed::{Engine, Outcome, execute};

fn err_code(db: &mut Engine, sql: &str) -> String {
    match execute(db, sql) {
        Err(e) => e.code().to_string(),
        Ok(_) => panic!("expected error for {sql}"),
    }
}

fn one(db: &mut Engine, sql: &str) -> Value {
    match execute(db, sql).unwrap() {
        Outcome::Query { rows, .. } => rows[0][0].clone(),
        other => panic!("expected query, got {other:?}"),
    }
}

/// The 16 raw bytes of 550e8400-e29b-41d4-a716-446655440000.
const UUID16: [u8; 16] = [
    0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44, 0x00, 0x00,
];

/// uuid → bytea is the 16 raw bytes (PG: 42846 — jed adds this cast).
#[test]
fn uuid_to_bytea_is_the_16_bytes() {
    let mut db = Engine::new();
    assert_eq!(
        one(
            &mut db,
            "SELECT '550e8400-e29b-41d4-a716-446655440000'::uuid::bytea"
        ),
        Value::Bytea(UUID16.to_vec())
    );
}

/// bytea → uuid takes the 16 raw bytes (PG: 42846 — jed adds this cast). The input bytea is the
/// hyphen-less hex; the result renders as the canonical lowercase uuid.
#[test]
fn bytea_to_uuid_is_the_16_bytes() {
    let mut db = Engine::new();
    assert_eq!(
        one(
            &mut db,
            "SELECT '\\x550e8400e29b41d4a716446655440000'::bytea::uuid"
        ),
        Value::Uuid(UUID16)
    );
}

/// bytea → uuid requires EXACTLY 16 bytes; any other length traps 22P02 (the wrong-width body —
/// there is no PG code to match, so jed reuses invalid_text_representation).
#[test]
fn bytea_to_uuid_wrong_length_traps_22p02() {
    let mut db = Engine::new();
    assert_eq!(err_code(&mut db, "SELECT '\\xabcd'::bytea::uuid"), "22P02"); // 2 bytes
    assert_eq!(err_code(&mut db, "SELECT '\\x'::bytea::uuid"), "22P02"); // empty (0 bytes)
    // 17 bytes (one too many)
    assert_eq!(
        err_code(
            &mut db,
            "SELECT '\\x550e8400e29b41d4a71644665544000000'::bytea::uuid"
        ),
        "22P02"
    );
}

/// The casts round-trip through real columns (the runtime, non-constant path): a uuid column → bytea
/// equals the bytea column, and the bytea column → uuid equals the uuid column. NULL adapts.
#[test]
fn uuid_bytea_round_trip_through_columns() {
    let mut db = Engine::new();
    execute(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, u uuid, b bytea)",
    )
    .unwrap();
    execute(
        &mut db,
        "INSERT INTO t VALUES (1, '550e8400-e29b-41d4-a716-446655440000', \
         '\\x550e8400e29b41d4a716446655440000'), (2, NULL, NULL)",
    )
    .unwrap();
    assert_eq!(
        one(&mut db, "SELECT u::bytea FROM t WHERE id = 1"),
        Value::Bytea(UUID16.to_vec())
    );
    assert_eq!(
        one(&mut db, "SELECT b::uuid FROM t WHERE id = 1"),
        Value::Uuid(UUID16)
    );
    // a NULL uuid / bytea adapts to a NULL of the target type
    assert_eq!(
        one(&mut db, "SELECT u::bytea FROM t WHERE id = 2"),
        Value::Null
    );
    assert_eq!(
        one(&mut db, "SELECT b::uuid FROM t WHERE id = 2"),
        Value::Null
    );
}

/// text → uuid / uuid → text smoke check (the oracle-corpus behavior, run here per core too): the
/// runtime text→uuid parses PG-flexibly (22P02 on malformed), uuid→text renders canonical lowercase.
#[test]
fn text_uuid_smoke() {
    let mut db = Engine::new();
    execute(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, s text, u uuid)",
    )
    .unwrap();
    // an UPPERCASE text value casts to the same 16 bytes and renders lowercase
    execute(
        &mut db,
        "INSERT INTO t VALUES (1, '550E8400-E29B-41D4-A716-446655440000', \
         '550e8400-e29b-41d4-a716-446655440000')",
    )
    .unwrap();
    assert_eq!(
        one(&mut db, "SELECT s::uuid FROM t WHERE id = 1"),
        Value::Uuid(UUID16)
    );
    assert_eq!(
        one(&mut db, "SELECT u::text FROM t WHERE id = 1"),
        Value::Text("550e8400-e29b-41d4-a716-446655440000".to_string())
    );
    // a malformed runtime text → uuid traps 22P02 (not a literal — the column path)
    execute(&mut db, "INSERT INTO t VALUES (2, 'not-a-uuid', NULL)").unwrap();
    assert_eq!(
        err_code(&mut db, "SELECT s::uuid FROM t WHERE id = 2"),
        "22P02"
    );
}
