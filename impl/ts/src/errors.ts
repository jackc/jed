// Structured error codes (CLAUDE.md §5, §10). A SqlState's code() is the canonical
// 5-char SQLSTATE from spec/errors/registry.toml (cross-checked in tests). Errors are
// thrown as EngineError (the TS idiom); the harness reads `.code()` to match
// `statement error <code>`.
//
// SqlState is a string-literal union (not a TS enum — the elidable subset forbids
// enums), and the union member IS the canonical snake_case name from the registry.

export type SqlState =
  | "numeric_value_out_of_range" // 22003 — integer overflow (CLAUDE.md §8)
  | "division_by_zero" // 22012 — division or modulo by zero
  | "invalid_row_count_in_limit_clause" // 2201W — a negative LIMIT count
  | "invalid_row_count_in_offset_clause" // 2201X — a negative OFFSET count
  | "not_null_violation" // 23502
  | "unique_violation" // 23505 — primary-key uniqueness
  | "syntax_error" // 42601
  | "undefined_table" // 42P01
  | "undefined_column" // 42703
  | "undefined_object" // 42704 — e.g. an unknown type name
  | "invalid_column_reference" // 42P10 — SELECT DISTINCT ORDER BY key not in select list
  | "datatype_mismatch" // 42804
  | "duplicate_table" // 42P07
  | "duplicate_column" // 42701
  | "invalid_table_definition" // 42P16 — e.g. more than one primary key
  | "feature_not_supported" // 0A000
  | "data_corrupted"; // XX001 — a malformed on-disk database file (CLAUDE.md §8)

const CODES: Record<SqlState, string> = {
  numeric_value_out_of_range: "22003",
  division_by_zero: "22012",
  invalid_row_count_in_limit_clause: "2201W",
  invalid_row_count_in_offset_clause: "2201X",
  not_null_violation: "23502",
  unique_violation: "23505",
  syntax_error: "42601",
  undefined_table: "42P01",
  undefined_column: "42703",
  undefined_object: "42704",
  invalid_column_reference: "42P10",
  datatype_mismatch: "42804",
  duplicate_table: "42P07",
  duplicate_column: "42701",
  invalid_table_definition: "42P16",
  feature_not_supported: "0A000",
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
