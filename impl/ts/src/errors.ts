// Structured error codes (CLAUDE.md §5, §10). A SqlState's code() is the canonical
// 5-char SQLSTATE from spec/errors/registry.toml (cross-checked in tests). Errors are
// thrown as EngineError (the TS idiom); the harness reads `.code()` to match
// `statement error <code>`.
//
// SqlState is a string-literal union (not a TS enum — the elidable subset forbids
// enums), and the union member IS the canonical snake_case name from the registry.

export type SqlState =
  | "cardinality_violation" // 21000 — a scalar subquery used as an expression returned >1 row (§26)
  | "data_exception" // 22000 — bare class-22 (array_append/prepend on a multidim array; array-functions.md §3.2)
  | "numeric_value_out_of_range" // 22003 — integer overflow (CLAUDE.md §8)
  | "null_value_not_allowed" // 22004 — array_position's start subscript must not be NULL (array-functions.md §8)
  | "invalid_datetime_format" // 22007 — malformed timestamp/timestamptz input
  | "datetime_field_overflow" // 22008 — out-of-range datetime field or value beyond i64 µs
  | "division_by_zero" // 22012 — division or modulo by zero
  | "invalid_parameter_value" // 22023 — a bad numeric typmod (e.g. numeric(0))
  | "sequence_generator_limit_exceeded" // 2200H — nextval advanced past a sequence's MAX/MINVALUE bound without CYCLE (sequences.md §4)
  | "array_subscript_error" // 2202E — non-matching multidim sub-array dims, or inverted [l:u] array bounds
  | "invalid_text_representation" // 22P02 — malformed text input (e.g. bytea hex)
  | "invalid_escape_sequence" // 22025 — a LIKE pattern ending in a lone escape character
  | "invalid_row_count_in_limit_clause" // 2201W — a negative LIMIT count
  | "invalid_row_count_in_offset_clause" // 2201X — a negative OFFSET count
  | "not_null_violation" // 23502
  | "foreign_key_violation" // 23503 — a child INSERT/UPDATE whose key is absent in the parent, or a parent DELETE/UPDATE of a still-referenced row (constraints.md §6)
  | "unique_violation" // 23505 — primary-key uniqueness
  | "check_violation" // 23514 — a candidate row falsified a CHECK expression (constraints.md §4)
  | "active_sql_transaction" // 25001 — a nested BEGIN (no SAVEPOINT this slice; transactions.md §4.2)
  | "read_only_sql_transaction" // 25006 — a write in a READ ONLY transaction (transactions.md §4.3)
  | "in_failed_sql_transaction" // 25P02 — a statement in a failed/aborted block (transactions.md §6)
  | "object_not_in_prerequisite_state" // 55000 — currval/lastval before nextval defined it this session (sequences.md §6)
  | "syntax_error" // 42601
  | "undefined_table" // 42P01
  | "undefined_column" // 42703
  | "ambiguous_column" // 42702 — a bare column matching more than one relation in scope (§15)
  | "undefined_object" // 42704 — e.g. an unknown type name
  | "invalid_column_reference" // 42P10 — SELECT DISTINCT ORDER BY key not in select list
  | "datatype_mismatch" // 42804
  | "invalid_foreign_key" // 42830 — referenced columns are not the parent's PK or a UNIQUE set, or the referencing/referenced column counts disagree (constraints.md §6.2)
  | "duplicate_table" // 42P07
  | "duplicate_column" // 42701
  | "duplicate_alias" // 42712 — two FROM relations share a label (a self-join needs aliases; §15)
  | "invalid_table_definition" // 42P16 — e.g. more than one primary key
  | "grouping_error" // 42803 — non-aggregated column not in GROUP BY, or aggregate in WHERE/ON/nested
  | "undefined_function" // 42883 — an unknown function name in a call (aggregates.md §5)
  | "indeterminate_datatype" // 42P18 — a bind parameter $N whose type cannot be inferred (api.md §5)
  | "undefined_parameter" // 42P02 — a bind parameter $N where none can exist (a CHECK expression)
  | "duplicate_object" // 42710 — a constraint name already taken on this table (constraints.md §4.3)
  | "wrong_object_type" // 42809 — DROP TABLE of an index name / DROP INDEX of a table name (indexes.md §2)
  | "name_too_long" // 42622 — an identifier exceeds MAX_IDENTIFIER_LENGTH (63 bytes); lexer gate (cost.md §7)
  | "generated_always" // 428C9 — a write supplying an explicit value to a GENERATED ALWAYS identity column without OVERRIDING SYSTEM VALUE (an INSERT), or assigning one (an UPDATE) (sequences.md §13)
  | "dependent_objects_still_exist" // 2BP01 — DROP TYPE ... RESTRICT of a composite still referenced (composite.md §7)
  | "feature_not_supported" // 0A000
  | "program_limit_exceeded" // 54000 — input SQL exceeds the per-handle max_sql_length; input-size gate (cost.md §7)
  | "statement_too_complex" // 54001 — nesting depth exceeds MAX_EXPR_DEPTH; native-stack gate (cost.md §7)
  | "cost_limit_exceeded" // 54P01 — accrued cost reached the caller-set max_cost ceiling (cost.md §6)
  | "io_error" // 58030 — an I/O error from the host file layer (spec/design/api.md §2)
  | "undefined_file" // 58P01 — open of a database path that does not exist
  | "duplicate_file" // 58P02 — create of a database path that already exists
  | "data_corrupted"; // XX001 — a malformed on-disk database file (CLAUDE.md §8)

const CODES: Record<SqlState, string> = {
  cardinality_violation: "21000",
  data_exception: "22000",
  numeric_value_out_of_range: "22003",
  null_value_not_allowed: "22004",
  invalid_datetime_format: "22007",
  datetime_field_overflow: "22008",
  division_by_zero: "22012",
  invalid_parameter_value: "22023",
  sequence_generator_limit_exceeded: "2200H",
  array_subscript_error: "2202E",
  invalid_text_representation: "22P02",
  invalid_escape_sequence: "22025",
  invalid_row_count_in_limit_clause: "2201W",
  invalid_row_count_in_offset_clause: "2201X",
  not_null_violation: "23502",
  foreign_key_violation: "23503",
  unique_violation: "23505",
  check_violation: "23514",
  active_sql_transaction: "25001",
  read_only_sql_transaction: "25006",
  in_failed_sql_transaction: "25P02",
  object_not_in_prerequisite_state: "55000",
  syntax_error: "42601",
  undefined_table: "42P01",
  undefined_column: "42703",
  ambiguous_column: "42702",
  undefined_object: "42704",
  invalid_column_reference: "42P10",
  datatype_mismatch: "42804",
  invalid_foreign_key: "42830",
  duplicate_table: "42P07",
  duplicate_column: "42701",
  duplicate_alias: "42712",
  invalid_table_definition: "42P16",
  grouping_error: "42803",
  undefined_function: "42883",
  indeterminate_datatype: "42P18",
  undefined_parameter: "42P02",
  duplicate_object: "42710",
  wrong_object_type: "42809",
  name_too_long: "42622",
  generated_always: "428C9",
  dependent_objects_still_exist: "2BP01",
  feature_not_supported: "0A000",
  program_limit_exceeded: "54000",
  statement_too_complex: "54001",
  cost_limit_exceeded: "54P01",
  io_error: "58030",
  undefined_file: "58P01",
  duplicate_file: "58P02",
  data_corrupted: "XX001",
};

// sqlStateCode returns the canonical SQLSTATE string for a state.
export function sqlStateCode(state: SqlState): string {
  return CODES[state];
}

// EngineError is an engine error: a SQLSTATE plus an informational (never-matched)
// message. The message text embeds the code so it also matches as a plain regex under
// a stock sqllogictest runner (spec/design/conformance.md §2).
export class EngineError extends Error {
  state: SqlState;

  constructor(state: SqlState, message: string) {
    super(`${CODES[state]}: ${message}`);
    this.name = "EngineError";
    this.state = state;
  }

  // code returns the error's SQLSTATE string.
  code(): string {
    return CODES[this.state];
  }
}

// engineError builds an EngineError (mirrors Go's NewError / Rust's EngineError::new).
export function engineError(state: SqlState, message: string): EngineError {
  return new EngineError(state, message);
}
