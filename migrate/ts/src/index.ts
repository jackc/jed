// jed-migrate — a small, opt-in schema-migration library for jed, modeled on tern and driven
// entirely through jed's public host API (../design.md). It links the TS core in; it is never
// part of the engine core.
//
// A migrations directory is a flat set of files named <sequence>_<name>.sql, each holding an
// up migration and an optional down migration separated by the magic line
//
//   ---- create above / drop below ----
//
// Sequence numbers are 1-based and contiguous (1 … N); the sequence is the version. Schema
// state is a single-integer high-water mark in a version table (default "schema_version").
//
// Typical use:
//
//   import { createDatabase } from "jed-ts";
//   import { loadMigrations, Migrator } from "jed-migrate";
//
//   const db = createDatabase({ path: "app.jed" });
//   const migrations = loadMigrations("migrations");
//   const m = new Migrator(db, migrations);
//   try {
//     m.migrate(); // bring the database up to the latest version
//   } finally {
//     m.close();
//   }

export {
  BadVersionError,
  IrreversibleMigrationError,
  LoadError,
  MigrationError,
} from "./errors.ts";
export { loadMigrations, loadMigrationsFromEntries } from "./load.ts";
export { isIrreversible, type Migration, SEPARATOR } from "./migration.ts";
export { DEFAULT_VERSION_TABLE, Migrator, type Options, type Status } from "./migrator.ts";
export { newMigration } from "./scaffold.ts";
export { resolveTargets } from "./target.ts";
