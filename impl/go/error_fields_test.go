package jed

// Structured error fields (spec/design/error-fields.md) — the ConstraintName / TableName /
// ColumnName / DataTypeName diagnostics on EngineError, modeled on pgx's pgconn.PgError. Out of
// the conformance corpus's reach (it matches on code/prose, never on a structured field —
// CLAUDE.md §10), so this is the host-API surface test. Mirrors impl/rust/tests/error_fields.rs
// and impl/ts/tests/error_fields.test.ts.

import "testing"

func efErr(t *testing.T, db dbHandle, sql string) *EngineError {
	t.Helper()
	_, err := queryOutcome(db, sql, nil)
	if err == nil {
		t.Fatalf("expected an error from %q", sql)
	}
	return err.(*EngineError)
}

func efEq(t *testing.T, got, want, field string) {
	t.Helper()
	if got != want {
		t.Errorf("%s = %q, want %q", field, got, want)
	}
}

// 23505 on a PRIMARY KEY reports the derived <table>_pkey constraint + the table; the rendered
// message is unchanged (fields are additive metadata, not a message change).
func TestErrorFieldsUniqueViolationPrimaryKey(t *testing.T) {
	db := fkSetup(t, "CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1)")
	e := efErr(t, db, "INSERT INTO t VALUES (1)")
	efEq(t, e.Code(), "23505", "code")
	efEq(t, e.ConstraintName, "t_pkey", "ConstraintName")
	efEq(t, e.TableName, "t", "TableName")
	efEq(t, e.ColumnName, "", "ColumnName")
	efEq(t, e.Message, "duplicate key value violates unique constraint: t_pkey", "Message")
}

// 23505 on a named UNIQUE index reports the index (= constraint) name.
func TestErrorFieldsUniqueViolationSecondaryIndex(t *testing.T) {
	db := fkSetup(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, email text)",
		"CREATE UNIQUE INDEX t_email_key ON t (email)",
		"INSERT INTO t VALUES (1, 'a')")
	e := efErr(t, db, "INSERT INTO t VALUES (2, 'a')")
	efEq(t, e.Code(), "23505", "code")
	efEq(t, e.ConstraintName, "t_email_key", "ConstraintName")
	efEq(t, e.TableName, "t", "TableName")
}

// 23514 reports the CHECK constraint + the relation.
func TestErrorFieldsCheckViolation(t *testing.T) {
	db := fkSetup(t, "CREATE TABLE t (id i32 PRIMARY KEY, n i32 CONSTRAINT n_pos CHECK (n > 0))")
	e := efErr(t, db, "INSERT INTO t VALUES (1, -1)")
	efEq(t, e.Code(), "23514", "code")
	efEq(t, e.ConstraintName, "n_pos", "ConstraintName")
	efEq(t, e.TableName, "t", "TableName")
}

// 23503 (child side) reports the FK constraint + the written table.
func TestErrorFieldsForeignKeyViolationInsert(t *testing.T) {
	db := fkSetup(t,
		"CREATE TABLE p (id i32 PRIMARY KEY)",
		"CREATE TABLE c (id i32 PRIMARY KEY, pid i32 CONSTRAINT c_pid_fk REFERENCES p)")
	e := efErr(t, db, "INSERT INTO c VALUES (1, 99)")
	efEq(t, e.Code(), "23503", "code")
	efEq(t, e.ConstraintName, "c_pid_fk", "ConstraintName")
	efEq(t, e.TableName, "c", "TableName")
}

// 23503 (parent side) reports the FK constraint + the modified (parent) table.
func TestErrorFieldsForeignKeyViolationDelete(t *testing.T) {
	db := fkSetup(t,
		"CREATE TABLE p (id i32 PRIMARY KEY)",
		"CREATE TABLE c (id i32 PRIMARY KEY, pid i32 CONSTRAINT c_pid_fk REFERENCES p)",
		"INSERT INTO p VALUES (1)",
		"INSERT INTO c VALUES (1, 1)")
	e := efErr(t, db, "DELETE FROM p WHERE id = 1")
	efEq(t, e.Code(), "23503", "code")
	efEq(t, e.ConstraintName, "c_pid_fk", "ConstraintName")
	efEq(t, e.TableName, "p", "TableName")
}

// 23P01 reports the EXCLUDE constraint (its backing GiST index name) + the table.
func TestErrorFieldsExclusionViolation(t *testing.T) {
	db := fkSetup(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, r i32range, CONSTRAINT t_r_excl EXCLUDE USING gist (r WITH &&))",
		"INSERT INTO t VALUES (1, '[1,5)')")
	e := efErr(t, db, "INSERT INTO t VALUES (2, '[3,8)')")
	efEq(t, e.Code(), "23P01", "code")
	efEq(t, e.ConstraintName, "t_r_excl", "ConstraintName")
	efEq(t, e.TableName, "t", "TableName")
}

// 23502 reports the column (unnamed constraint, as in PostgreSQL); the table is stamped at the DML
// boundary.
func TestErrorFieldsNotNullViolation(t *testing.T) {
	db := fkSetup(t, "CREATE TABLE t (id i32 PRIMARY KEY, n i32 NOT NULL)")
	e := efErr(t, db, "INSERT INTO t VALUES (1, NULL)")
	efEq(t, e.Code(), "23502", "code")
	efEq(t, e.ColumnName, "n", "ColumnName")
	efEq(t, e.TableName, "t", "TableName")
	efEq(t, e.ConstraintName, "", "ConstraintName")
}

// 22003 (integer overflow on column store) reports the data type + the table.
func TestErrorFieldsNumericValueOutOfRange(t *testing.T) {
	db := fkSetup(t, "CREATE TABLE t (id i32 PRIMARY KEY, n i16)")
	e := efErr(t, db, "INSERT INTO t VALUES (1, 99999)")
	efEq(t, e.Code(), "22003", "code")
	efEq(t, e.DataTypeName, "i16", "DataTypeName")
	efEq(t, e.TableName, "t", "TableName")
}

// 22001 (varchar length) reports the type + the column.
func TestErrorFieldsStringDataRightTruncation(t *testing.T) {
	db := fkSetup(t, "CREATE TABLE t (id i32 PRIMARY KEY, s varchar(3))")
	e := efErr(t, db, "INSERT INTO t VALUES (1, 'abcd')")
	efEq(t, e.Code(), "22001", "code")
	efEq(t, e.DataTypeName, "varchar(3)", "DataTypeName")
	efEq(t, e.ColumnName, "s", "ColumnName")
	efEq(t, e.TableName, "t", "TableName")
}

// A non-constraint error leaves every structured field unset.
func TestErrorFieldsUnrelatedErrorHasNoFields(t *testing.T) {
	db := fkSetup(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	e := efErr(t, db, "SELECT nonesuch FROM t")
	efEq(t, e.ConstraintName, "", "ConstraintName")
	efEq(t, e.TableName, "", "TableName")
	efEq(t, e.ColumnName, "", "ColumnName")
	efEq(t, e.DataTypeName, "", "DataTypeName")
}
