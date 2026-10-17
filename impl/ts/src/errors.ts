// Structured error codes (CLAUDE.md §5, §10). A SqlState's code is the canonical 5-char
// SQLSTATE from spec/errors/registry.toml. The SqlState union + the code mapping are
// generated (the codegen "middle path", CLAUDE.md §5 — see sqlstate.ts /
// spec/design/codegen.md); this file is the hand-written EngineError scaffolding that
// consumes them. Errors are thrown as EngineError (the TS idiom); the harness reads
// `.code()` to match `statement error <code>`.

import { type SqlState, sqlStateCode } from "./sqlstate.ts";

// Re-export so existing `./errors.ts` consumers (and lib.ts) keep their import paths.
export { type SqlState, sqlStateCode };

// EngineError is an engine error: a SQLSTATE plus an informational (never-matched)
// message. The message text embeds the code so it also matches as a plain regex under
// a stock sqllogictest runner (spec/design/conformance.md §2).
//
// Beyond code + message it carries optional structured diagnostic fields
// (spec/design/error-fields.md §3) modeled on pgx's pgconn.PgError — the constraint /
// table / column / data-type name a host would otherwise scrape from the message text.
// An `undefined` field means it does not apply to this error.
export class EngineError extends Error {
  state: SqlState;
  constraintName?: string;
  tableName?: string;
  // The database holding `tableName` — `main`, `temp`, or a host attachment's name. jed's analog of
  // PostgreSQL's SchemaName: set whenever `tableName` is (spec/design/error-fields.md §12).
  databaseName?: string;
  columnName?: string;
  dataTypeName?: string;
  // PostgreSQL's DETAIL sentence naming the offending values (`Key (id)=(1) already exists.`) — set
  // on the class-23 violations, subject to the session's SELECT privilege
  // (spec/design/error-fields.md §9). Never part of the rendered message.
  detail?: string;
  // The 1-based CHARACTER (code point) offset into the statement text of the offending token — set
  // for lexer/parser errors and name-resolution errors (spec/design/error-fields.md §10).
  position?: number;
  // PostgreSQL's HINT suggestion (`Perhaps you meant to reference the column "t.name".`) —
  // spec/design/error-fields.md §11. Never part of the rendered message.
  hint?: string;
  // Set once an expression node's resolution has decided this error's position (stamped or
  // deliberately left absent), so an enclosing node never claims an error raised inside one of its
  // operands (spec/design/error-fields.md §10.2). Internal bookkeeping, not a field.
  located?: boolean;
  // Set once a raise site has decided this error's Hint (set it, or deliberately left it absent), so
  // an enclosing context never adds its default hint (spec/design/error-fields.md §11.7). Internal
  // bookkeeping, not a field.
  hintDecided?: boolean;
  // The INSERT VALUES slot [row, value] whose stored value raised this error, set where the row is
  // built so the statement can point the error at the slot's literal (spec/design/error-fields.md
  // §10.2). Internal bookkeeping, not a field.
  slot?: [number, number];

  constructor(state: SqlState, message: string) {
    super(`${sqlStateCode(state)}: ${message}`);
    this.name = "EngineError";
    this.state = state;
  }

  // code returns the error's SQLSTATE string.
  code(): string {
    return sqlStateCode(this.state);
  }

  // --- structured-field builders (spec/design/error-fields.md §5) --------------------
  // Chainable setters. engineError() leaves every field undefined, so existing call sites
  // are unaffected; only the raise sites that know an identifier opt in.
  withConstraint(name: string): this {
    this.constraintName = name;
    return this;
  }
  withTable(name: string): this {
    this.tableName = name;
    return this;
  }
  // withDatabase stamps DatabaseName with the database holding the error's table
  // (spec/design/error-fields.md §12): set only when TableName is set and no inner site already
  // named the database.
  withDatabase(database: string): this {
    if (this.tableName !== undefined && this.databaseName === undefined) {
      this.databaseName = database;
    }
    return this;
  }
  withColumn(name: string): this {
    this.columnName = name;
    return this;
  }
  withDataType(name: string): this {
    this.dataTypeName = name;
    return this;
  }
  // withDetail attaches a Detail (spec/design/error-fields.md §9). undefined (a gated or
  // inapplicable detail) leaves the field unset, so a raise site can pass a helper's result straight
  // through.
  withDetail(detail: string | undefined): this {
    this.detail = detail;
    return this;
  }
  // withPosition sets Position unless one is already set: the innermost site that knows a location
  // wins (spec/design/error-fields.md §10.2). A position of 0 (or undefined) means "unknown" and is
  // ignored.
  withPosition(pos: number | undefined): this {
    if (this.position === undefined && pos !== undefined && pos > 0) this.position = pos;
    return this;
  }
  // locate stamps an error leaving an expression node's resolution with pos unless an inner node
  // already decided its position, then marks it decided (spec/design/error-fields.md §10.2).
  locate(pos: number | undefined): this {
    if (this.located !== true) {
      this.withPosition(pos);
      this.located = true;
    }
    return this;
  }
  // atTarget positions a DML target's 42P01 at the target's name (spec/design/error-fields.md
  // §10.2); any other error passes through.
  atTarget(pos: number | undefined): this {
    if (this.state === "undefined_table") this.withPosition(pos);
    return this;
  }
  // shiftPosition moves Position by `chars` characters — a statement's error moved into the
  // coordinates of the script that contains it (spec/design/error-fields.md §10.1).
  shiftPosition(chars: number): this {
    if (this.position !== undefined) this.position += chars;
    return this;
  }
  // atSlot records the VALUES slot `value` (of row 0) a row-building error came from (§10.2).
  atSlot(value: number): this {
    if (this.slot === undefined) this.slot = [0, value];
    return this;
  }
  // atRow moves a recorded slot to VALUES row `row` (§10.2).
  atRow(row: number): this {
    if (this.slot !== undefined) this.slot = [row, this.slot[1]];
    return this;
  }
  // withHint sets Hint unless one is already set (spec/design/error-fields.md §11).
  withHint(hint: string | undefined): this {
    if (this.hint === undefined) this.hint = hint;
    return this;
  }
  // hinted sets the fixed Hint `text` and marks the hint decided (spec/design/error-fields.md §11.7).
  hinted(text: string): this {
    this.withHint(text);
    this.hintDecided = true;
    return this;
  }
  // noHint decides that this error carries no Hint, so no enclosing context adds one (§11.7).
  noHint(): this {
    this.hintDecided = true;
    return this;
  }
  // defaultHint is an enclosing context's default Hint (§11.7): set `text` unless the raise site
  // already decided the hint.
  defaultHint(text: string): this {
    if (this.hintDecided === true || this.hint !== undefined) return this;
    return this.hinted(text);
  }
}

// positionedAt sets an EngineError's Position at pos unless one is already set — a raise site that
// knows the offending expression (spec/design/error-fields.md §10.2). Any other error passes through.
export function positionedAt<E>(err: E, pos: number | undefined): E {
  if (err instanceof EngineError) err.withPosition(pos);
  return err;
}

// atPosition runs f, positioning an EngineError it throws at pos (positionedAt).
export function atPosition<T>(pos: number | undefined, f: () => T): T {
  try {
    return f();
  } catch (err) {
    throw positionedAt(err, pos);
  }
}

// engineError builds an EngineError (mirrors Go's NewError / Rust's EngineError::new).
export function engineError(state: SqlState, message: string): EngineError {
  return new EngineError(state, message);
}

// stampTable annotates a column-store failure (23502 / 22003 / 22001) with the target
// relation: the relation name is in scope at the DML boundary, not inside the coercion
// (spec/design/error-fields.md §4). A no-op for a non-EngineError. Rethrow the result.
export function stampTable(err: unknown, table: string): unknown {
  if (err instanceof EngineError) err.tableName = table;
  return err;
}

// pkeyName (the derived <table>_pkey constraint name) lives with the other engine auto-names.
export { pkeyName } from "./autoname.ts";

// --- typed integrity-violation constructors (spec/design/error-fields.md §5) ----------
// One source per message template (mirrors spec/errors/registry.toml), so the prose and the
// structured field can never drift apart. Message text is byte-identical to the prior inline
// concatenations — the conformance corpus matches on it.

// uniqueViolation — 23505 for a duplicate key under `constraint` (a unique index's name, or
// the derived <table>_pkey) on `table`.
export function uniqueViolation(table: string, constraint: string): EngineError {
  return engineError(
    "unique_violation",
    "duplicate key value violates unique constraint: " + constraint,
  )
    .withTable(table)
    .withConstraint(constraint);
}

// checkViolation — 23514, a row fails CHECK `constraint` on `table`.
export function checkViolation(table: string, constraint: string): EngineError {
  return engineError(
    "check_violation",
    "new row for relation " + table + " violates check constraint " + constraint,
  )
    .withTable(table)
    .withConstraint(constraint);
}

// fkViolationInsert — 23503 child side, an INSERT/UPDATE on `table` references a parent key
// absent under FK `constraint`.
export function fkViolationInsert(table: string, constraint: string): EngineError {
  return engineError(
    "foreign_key_violation",
    "insert or update on table " + table + " violates foreign key constraint " + constraint,
  )
    .withTable(table)
    .withConstraint(constraint);
}

// fkViolationMatchFull — 23503 child side under MATCH FULL, an INSERT/UPDATE on `table` writes a
// referencing tuple that mixes NULL and non-NULL values under FK `constraint` (constraints.md §6.3).
// PostgreSQL's DETAIL names no values, so it is never privilege-gated.
export function fkViolationMatchFull(table: string, constraint: string): EngineError {
  return fkViolationInsert(table, constraint).withDetail(
    "MATCH FULL does not allow mixing of null and nonnull key values.",
  );
}

// fkViolationDelete — 23503 parent side, a DELETE/UPDATE on `parent` strands a child of
// `child` still referencing it under FK `constraint`.
export function fkViolationDelete(parent: string, constraint: string, child: string): EngineError {
  return engineError(
    "foreign_key_violation",
    "update or delete on table " +
      parent +
      " violates foreign key constraint " +
      constraint +
      " on table " +
      child,
  )
    .withTable(parent)
    .withConstraint(constraint);
}

// exclusionViolation — 23P01, a row conflicts with an existing one under EXCLUDE `constraint`
// (the backing GiST index's name) on `table`.
export function exclusionViolation(table: string, constraint: string): EngineError {
  return engineError(
    "exclusion_violation",
    "conflicting key value violates exclusion constraint: " + constraint,
  )
    .withTable(table)
    .withConstraint(constraint);
}

// generatedAlwaysInsert — 428C9, an INSERT supplies a non-DEFAULT value to GENERATED ALWAYS
// identity `column` without OVERRIDING SYSTEM VALUE, with PostgreSQL's Detail and Hint
// (error-fields.md §11.9).
export function generatedAlwaysInsert(column: string): EngineError {
  return engineError("generated_always", `cannot insert a non-DEFAULT value into column ${column}`)
    .withDetail(identityAlwaysDetail(column))
    .hinted("Use OVERRIDING SYSTEM VALUE to override.");
}

// generatedAlwaysUpdate — 428C9, an UPDATE assigns GENERATED ALWAYS identity `column` a value other
// than DEFAULT, with PostgreSQL's Detail (error-fields.md §11.9).
export function generatedAlwaysUpdate(column: string): EngineError {
  return engineError("generated_always", `column ${column} can only be updated to DEFAULT`)
    .withDetail(identityAlwaysDetail(column))
    .noHint();
}

// identityAlwaysDetail is PostgreSQL's 428C9 Detail for a GENERATED ALWAYS identity column
// (rewriteHandler.c).
function identityAlwaysDetail(column: string): string {
  return `Column "${column}" is an identity column defined as GENERATED ALWAYS.`;
}

// notNullViolation — 23502, a NULL into NOT NULL `column`. The table is stamped at the DML
// boundary (stampTable), where the relation name is in scope.
export function notNullViolation(column: string): EngineError {
  return engineError(
    "not_null_violation",
    "null value in column " + column + " violates not-null constraint",
  ).withColumn(column);
}
