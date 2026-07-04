// The structured error surface (design.md §8). MigrationError preserves the engine's
// EngineError underneath (as `cause`), so a caller can still branch on the SQLSTATE.

import { EngineError } from "../../../impl/ts/src/lib.ts";

// LoadError is a load-time failure: a malformed file, a gap or duplicate in the sequence
// numbers, an empty forward half, an invalid version-table name, or an unreadable source
// (design.md §7/§8). Raised before any statement runs.
export class LoadError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "LoadError";
  }
}

// MigrationError wraps an engine error raised while running a migration statement (design.md
// §8 — the tern MigrationPgError analogue). It records the migration name, the direction
// ("up"/"down"), and the failing statement text; the underlying EngineError is its `cause`.
export class MigrationError extends Error {
  readonly migration: string;
  readonly direction: string;
  readonly statement: string;

  constructor(migration: string, direction: string, statement: string, cause: EngineError) {
    super(
      `migration "${migration}" (${direction}) failed: ${cause.message}\n  in statement: ${statement}`,
      { cause },
    );
    this.name = "MigrationError";
    this.migration = migration;
    this.direction = direction;
    this.statement = statement;
  }

  // sqlState returns the underlying engine SQLSTATE, or undefined when the cause is not an
  // EngineError — a convenience so callers need not unwrap `cause` by hand.
  sqlState(): string | undefined {
    return this.cause instanceof EngineError ? this.cause.code() : undefined;
  }
}

// IrreversibleMigrationError is thrown when a down-migration is requested through a migration
// that has no down half (design.md §8).
export class IrreversibleMigrationError extends Error {
  readonly sequence: number;
  readonly migration: string;

  constructor(sequence: number, migration: string) {
    super(`migration ${sequence} ("${migration}") is irreversible: it has no down migration`);
    this.name = "IrreversibleMigrationError";
    this.sequence = sequence;
    this.migration = migration;
  }
}

// BadVersionError is thrown when a target version, or the version read from the table, is
// outside 0 … N (design.md §6/§8). `whence` is "target" or "database".
export class BadVersionError extends Error {
  readonly version: number;
  readonly n: number;
  readonly whence: string;

  constructor(version: number, n: number, whence: string) {
    super(`${whence} version ${version} is out of range 0 … ${n}`);
    this.name = "BadVersionError";
    this.version = version;
    this.n = n;
    this.whence = whence;
  }
}
