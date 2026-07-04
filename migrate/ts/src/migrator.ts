// The Migrator: the version table (design.md §5) and the migrate algorithm (design.md §6).

import {
  type Database,
  EngineError,
  type Session,
  splitStatements,
} from "../../../impl/ts/src/lib.ts";
import {
  BadVersionError,
  IrreversibleMigrationError,
  LoadError,
  MigrationError,
} from "./errors.ts";
import { validateSequence } from "./load.ts";
import { isIrreversible, type Migration } from "./migration.ts";

// DEFAULT_VERSION_TABLE is the version table name used when Options.versionTable is unset
// (design.md §5). There is no schema qualifier — jed has no schema namespace.
export const DEFAULT_VERSION_TABLE = "schema_version";

// A safe (optionally attached-db-qualified) identifier — the version table is interpolated
// into SQL, so validating it keeps the interpolation safe by construction.
const VERSION_TABLE_PATTERN = /^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)?$/;

// Options configures a Migrator.
export type Options = {
  // versionTable overrides the default "schema_version". May be a bare name or a name
  // qualified by an attached-database name ("reports.schema_version").
  versionTable?: string;
};

// Status is the result of Migrator.status.
export type Status = {
  current: number; // the version recorded in the version table
  target: number; // the latest available version (N)
  pending: number; // how many migrations are not yet applied (target - current, clamped at 0)
};

// Migrator applies a set of migrations to a jed database, tracking progress in a
// single-integer version table (design.md §5/§6). It owns an internal read-write session for
// its lifetime; call close() when done to release it.
export class Migrator {
  private readonly session: Session;
  private readonly _migrations: Migration[];
  private readonly _versionTable: string;

  // Build a Migrator over db and the (already loaded, e.g. via loadMigrations) migrations. It
  // mints one internal read-write session that every step runs on — load-bearing: jed's bare
  // Database convenience methods mint a fresh session per call (session.md §2.4), so a step's
  // schema change and its version bump must run on one persistent session to land in a single
  // transaction.
  constructor(db: Database, migrations: Migration[], opts: Options = {}) {
    const table = opts.versionTable ?? DEFAULT_VERSION_TABLE;
    if (!VERSION_TABLE_PATTERN.test(table)) {
      throw new LoadError(`invalid version table name "${table}"`);
    }
    const sorted = [...migrations].sort((a, b) => a.sequence - b.sequence);
    validateSequence(sorted);
    this.session = db.session();
    this._migrations = sorted;
    this._versionTable = table;
  }

  // close releases the Migrator's internal session. Idempotent.
  close(): void {
    this.session.close();
  }

  // migrations is the loaded migration set, ordered by sequence.
  get migrations(): readonly Migration[] {
    return this._migrations;
  }

  // versionTable is the version table name in use.
  get versionTable(): string {
    return this._versionTable;
  }

  // migrate brings the database up to the latest version (design.md §6) — the dominant
  // application-startup case, equivalent to migrateTo(migrations.length).
  migrate(): void {
    this.migrateTo(this._migrations.length);
  }

  // migrateTo brings the database to an absolute target version in 0 … N by stepping one
  // migration at a time (design.md §6). Each step is its own committed transaction, so an
  // interrupted run leaves the database at a clean intermediate version (resumable). A target
  // outside 0 … N, or a version-table value outside it, throws BadVersionError.
  migrateTo(target: number): void {
    this.ensureVersionTable();
    const n = this._migrations.length;
    if (target < 0 || target > n) throw new BadVersionError(target, n, "target");
    const current = this.readVersion();
    if (current < 0 || current > n) throw new BadVersionError(current, n, "database");
    if (current === target) return; // fast path: already there, no write transaction opened
    if (target > current) {
      for (let v = current + 1; v <= target; v++) this.up(v);
    } else {
      for (let v = current; v > target; v--) this.down(v);
    }
  }

  // status reports the current version, the target (N), and the number of pending migrations
  // (design.md §9). It ensures the version table exists first.
  status(): Status {
    const current = this.currentVersion();
    const n = this._migrations.length;
    return { current, target: n, pending: Math.max(0, n - current) };
  }

  // currentVersion ensures the version table exists, then reads and returns the current version.
  currentVersion(): number {
    this.ensureVersionTable();
    return this.readVersion();
  }

  // up applies migration v's up half, then bumps the version to v — one atomic step.
  private up(v: number): void {
    const mg = this._migrations[v - 1];
    this.runStep(mg.name, "up", mg.up, v);
  }

  // down applies migration v's down half, then bumps the version to v-1 — one atomic step. A
  // migration with no down half is irreversible.
  private down(v: number): void {
    const mg = this._migrations[v - 1];
    if (isIrreversible(mg)) throw new IrreversibleMigrationError(mg.sequence, mg.name);
    this.runStep(mg.name, "down", mg.down as string, v - 1);
  }

  // runStep runs one migration half plus the version bump in a single write transaction
  // (design.md §6). Each statement runs via executeScript joining the open transaction, which
  // rejects in-script BEGIN/COMMIT/ROLLBACK (0A000) so the schema change and the version bump
  // are one atomic unit. On any error the transaction is rolled back (the step made no change)
  // and a MigrationError naming the migration and failing statement is thrown.
  private runStep(name: string, direction: string, sql: string, newVersion: number): void {
    this.session.begin(true);
    for (const span of splitStatements(sql)) {
      try {
        this.session.executeScript(span.text);
      } catch (e) {
        this.session.rollback();
        if (e instanceof EngineError) {
          throw new MigrationError(name, direction, span.text, e);
        }
        throw e;
      }
    }
    try {
      this.session.executeScript(`update ${this._versionTable} set version = ${newVersion}`);
    } catch (e) {
      this.session.rollback();
      throw e;
    }
    this.session.commit();
  }

  // ensureVersionTable creates the version table (seeded with 0) if it does not already exist,
  // idempotently, in its own committed transaction (design.md §5). Safe to call repeatedly.
  private ensureVersionTable(): void {
    const t = this._versionTable;
    try {
      this.session.executeScript(`create table ${t} (version integer not null)`);
    } catch (e) {
      // A create against an existing table is duplicate_table (42P07) — tolerated so ensure is
      // idempotent; any other error is real. `state` is the typed SqlState union, so the
      // comparison is checked at compile time (a typo'd member fails tsc).
      if (!(e instanceof EngineError) || e.state !== "duplicate_table") throw e;
    }
    this.session.executeScript(
      `insert into ${t} (version) select 0 where not exists (select 1 from ${t})`,
    );
  }

  // readVersion reads the single high-water-mark row from the version table.
  private readVersion(): number {
    const row = this.session.get(`select version from ${this._versionTable}`);
    if (row === undefined) {
      throw new LoadError(`version table ${this._versionTable} has no row`);
    }
    return Number(row.version);
  }
}
