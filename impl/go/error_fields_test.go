package jed

// Structured error fields (spec/design/error-fields.md) — the ConstraintName / TableName /
// ColumnName / DataTypeName diagnostics on EngineError, modeled on pgx's pgconn.PgError. Out of
// the conformance corpus's reach (it matches on code/prose, never on a structured field —
// CLAUDE.md §10), so this is the host-API surface test. Mirrors impl/rust/tests/error_fields.rs
// and impl/ts/tests/error_fields.test.ts.

import (
	"errors"
	"fmt"
	"os"
	"strconv"
	"strings"
	"testing"
)

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
	efEq(t, e.Detail, "", "Detail")
}

// detailCase is one case of the shared Detail fixture (spec/errors/detail_cases.txt).
type detailCase struct {
	name              string
	setup             []string
	defaultPrivileges *string
	grants            [][2]string
	revokes           [][2]string
	run               string
	code              string
	detail            string // "" = the Detail must be absent (`no-detail`)
}

func parseDetailCases(t *testing.T, text string) []detailCase {
	t.Helper()
	var cases []detailCase
	privObj := func(rest string) [2]string {
		p, o, ok := strings.Cut(rest, " ")
		if !ok {
			t.Fatalf("PRIV object: %q", rest)
		}
		return [2]string{p, o}
	}
	for _, line := range strings.Split(text, "\n") {
		if strings.TrimSpace(line) == "" || strings.HasPrefix(line, "#") {
			continue
		}
		if name, ok := strings.CutPrefix(line, "case: "); ok {
			cases = append(cases, detailCase{name: name})
			continue
		}
		if len(cases) == 0 {
			t.Fatalf("a directive before the first case: %q", line)
		}
		c := &cases[len(cases)-1]
		if v, ok := strings.CutPrefix(line, "setup: "); ok {
			c.setup = append(c.setup, v)
		} else if v, ok := strings.CutPrefix(line, "default_privileges: "); ok {
			c.defaultPrivileges = &v
		} else if v, ok := strings.CutPrefix(line, "grant: "); ok {
			c.grants = append(c.grants, privObj(v))
		} else if v, ok := strings.CutPrefix(line, "revoke: "); ok {
			c.revokes = append(c.revokes, privObj(v))
		} else if v, ok := strings.CutPrefix(line, "run: "); ok {
			c.run = v
		} else if v, ok := strings.CutPrefix(line, "code: "); ok {
			c.code = v
		} else if v, ok := strings.CutPrefix(line, "detail: "); ok {
			c.detail = v
		} else if line != "no-detail" {
			t.Fatalf("unknown detail_cases directive: %q", line)
		}
	}
	return cases
}

func detailPrivilegeSet(t *testing.T, list string) PrivilegeSet {
	t.Helper()
	set := PrivSetEmpty
	if list == "-" {
		return set
	}
	for _, name := range strings.Split(list, ",") {
		p, ok := PrivilegeFromName(name)
		if !ok {
			t.Fatalf("privilege name %q", name)
		}
		set = set.With(p)
	}
	return set
}

// The Detail field (spec/design/error-fields.md §9): every case of the shared, language-neutral
// fixture — the same file the Rust and TS cores run, so the text is pinned byte-identical.
func TestErrorFieldsDetailSharedCases(t *testing.T) {
	text, err := os.ReadFile("../../spec/errors/detail_cases.txt")
	if err != nil {
		t.Fatalf("read detail_cases.txt: %v", err)
	}
	cases := parseDetailCases(t, string(text))
	if len(cases) == 0 {
		t.Fatal("no detail cases")
	}
	var failures []string
	for _, c := range cases {
		db := fkSetup(t, c.setup...)
		if c.defaultPrivileges != nil {
			db.SetDefaultPrivileges(detailPrivilegeSet(t, *c.defaultPrivileges))
		}
		for _, g := range c.grants {
			db.Grant(detailPrivilegeSet(t, g[0]), g[1])
		}
		for _, r := range c.revokes {
			db.Revoke(detailPrivilegeSet(t, r[0]), r[1])
		}
		_, err := queryOutcome(db, c.run, nil)
		if err == nil {
			failures = append(failures, fmt.Sprintf("%s: expected %s but succeeded", c.name, c.code))
			continue
		}
		var e *EngineError
		if !errors.As(err, &e) {
			failures = append(failures, fmt.Sprintf("%s: non-engine error %v", c.name, err))
			continue
		}
		if e.Code() != c.code || e.Detail != c.detail {
			failures = append(failures, fmt.Sprintf("%s: got %s %q (%s), want %s %q",
				c.name, e.Code(), e.Detail, e.Message, c.code, c.detail))
		}
	}
	if len(failures) > 0 {
		t.Error(strings.Join(failures, "\n"))
	}
}

// positionCase is one case of the shared Position/Hint fixture (spec/errors/position_cases.txt).
type positionCase struct {
	name     string
	setup    []string
	run      string
	script   bool
	code     string
	position int32  // 0 = the Position must be absent (`no-position`)
	hint     string // "" = the Hint must be absent
}

func parsePositionCases(t *testing.T, text string) []positionCase {
	t.Helper()
	var cases []positionCase
	for _, line := range strings.Split(text, "\n") {
		if strings.TrimSpace(line) == "" || strings.HasPrefix(line, "#") {
			continue
		}
		if name, ok := strings.CutPrefix(line, "case: "); ok {
			cases = append(cases, positionCase{name: name})
			continue
		}
		if len(cases) == 0 {
			t.Fatalf("a directive before the first case: %q", line)
		}
		c := &cases[len(cases)-1]
		if v, ok := strings.CutPrefix(line, "setup: "); ok {
			c.setup = append(c.setup, v)
		} else if v, ok := strings.CutPrefix(line, "run: "); ok {
			c.run = v
		} else if v, ok := strings.CutPrefix(line, "script: "); ok {
			c.run = v
			c.script = true
		} else if v, ok := strings.CutPrefix(line, "code: "); ok {
			c.code = v
		} else if v, ok := strings.CutPrefix(line, "position: "); ok {
			n, err := strconv.ParseInt(v, 10, 32)
			if err != nil || n <= 0 {
				t.Fatalf("position_cases position: %q", v)
			}
			c.position = int32(n)
		} else if v, ok := strings.CutPrefix(line, "hint: "); ok {
			c.hint = v
		} else if line != "no-position" {
			t.Fatalf("unknown position_cases directive: %q", line)
		}
	}
	return cases
}

// The Position and Hint fields (spec/design/error-fields.md §10, §11): every case of the shared,
// language-neutral fixture — the same file the Rust and TS cores run.
func TestErrorFieldsPositionHintSharedCases(t *testing.T) {
	text, err := os.ReadFile("../../spec/errors/position_cases.txt")
	if err != nil {
		t.Fatalf("read position_cases.txt: %v", err)
	}
	cases := parsePositionCases(t, string(text))
	if len(cases) == 0 {
		t.Fatal("no position cases")
	}
	var failures []string
	for _, c := range cases {
		db := fkSetup(t, c.setup...)
		if c.script {
			_, err = db.ExecuteScript(c.run)
		} else {
			_, err = queryOutcome(db, c.run, nil)
		}
		if err == nil {
			failures = append(failures, fmt.Sprintf("%s: expected %s but succeeded", c.name, c.code))
			continue
		}
		var e *EngineError
		if !errors.As(err, &e) {
			failures = append(failures, fmt.Sprintf("%s: non-engine error %v", c.name, err))
			continue
		}
		if e.Code() != c.code || e.Position != c.position || e.Hint != c.hint {
			failures = append(failures, fmt.Sprintf("%s: got %s %d %q (%s), want %s %d %q",
				c.name, e.Code(), e.Position, e.Hint, e.Message, c.code, c.position, c.hint))
		}
	}
	if len(failures) > 0 {
		t.Error(strings.Join(failures, "\n"))
	}
}
