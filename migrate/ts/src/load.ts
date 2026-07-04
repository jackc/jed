// Loading migrations from a directory or an embedded set (design.md §7 — the source seam).

import { readdirSync, readFileSync, statSync } from "node:fs";
import { basename, join } from "node:path";
import { LoadError } from "./errors.ts";
import { type Migration, parseMigration } from "./migration.ts";

// FILE_NAME_PATTERN matches a migration file name: a decimal sequence prefix, an underscore, a
// free-form label, and the .sql extension (design.md §4). Files that do not match are ignored.
const FILE_NAME_PATTERN = /^(\d+)_.+\.sql$/;

// loadMigrations reads and validates the migrations directory at `dir` from the filesystem
// (design.md §7 — the default source). It returns the migrations ordered by sequence, or
// throws a LoadError if the set is malformed (a gap, a duplicate, an empty forward half)
// before any statement runs.
export function loadMigrations(dir: string): Migration[] {
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch (e) {
    throw new LoadError(`reading ${dir}: ${(e as Error).message}`);
  }
  const files: Array<[string, string]> = [];
  for (const name of names) {
    const full = join(dir, name);
    if (statSync(full).isDirectory()) continue;
    if (!FILE_NAME_PATTERN.test(name)) continue; // README, .bak, draft_*.sql, … — ignore
    files.push([name, readFileSync(full, "utf8")]);
  }
  return build(files);
}

// loadMigrationsFromEntries builds a validated migration set from an embedded set of
// name → contents entries (design.md §7 — the embedded source is first-class). It accepts a
// plain object (the shape of a bundler glob such as Vite's `import.meta.glob('./migrations/*.sql',
// { query: '?raw', eager: true, import: 'default' })`) or an array of [name, contents] pairs.
// Object keys may be full paths — only the basename is matched — so a bundler glob loads
// identically to a directory.
export function loadMigrationsFromEntries(
  entries: Record<string, string> | Array<[string, string]>,
): Migration[] {
  const pairs: Array<[string, string]> = Array.isArray(entries) ? entries : Object.entries(entries);
  const files: Array<[string, string]> = [];
  for (const [key, contents] of pairs) {
    const name = basename(key);
    if (!FILE_NAME_PATTERN.test(name)) continue; // ignore a non-migration name, like the dir loader
    files.push([name, contents]);
  }
  return build(files);
}

// build turns [name, contents] pairs into a validated, ordered migration set: detect duplicate
// sequences, parse each half, sort by sequence, and require the contiguous set 1 … N.
function build(files: Array<[string, string]>): Migration[] {
  const migrations: Migration[] = [];
  const seen = new Map<number, string>();
  for (const [name, contents] of files) {
    const seq = sequenceOf(name);
    if (seq === null) continue;
    const prev = seen.get(seq);
    if (prev !== undefined) {
      throw new LoadError(`duplicate sequence ${seq}: ${prev} and ${name}`);
    }
    seen.set(seq, name);
    migrations.push(parseMigration(seq, migrationLabel(name), contents));
  }
  migrations.sort((a, b) => a.sequence - b.sequence);
  validateSequence(migrations);
  return migrations;
}

// validateSequence checks that the migrations form the contiguous set 1 … N with no gaps
// (design.md §4/§7). The array must already be sorted by sequence.
export function validateSequence(migrations: Migration[]): void {
  for (let i = 0; i < migrations.length; i++) {
    const want = i + 1;
    const m = migrations[i];
    if (m.sequence !== want) {
      throw new LoadError(
        `non-contiguous migration sequence: expected ${want}, found ${m.sequence} (${m.name}); ` +
          "sequences must be 1 … N with no gaps",
      );
    }
  }
}

// sequenceOf returns the sequence number of a migration file name, or null if the name is not
// a migration file.
function sequenceOf(name: string): number | null {
  const match = FILE_NAME_PATTERN.exec(name);
  return match ? Number.parseInt(match[1], 10) : null;
}

// migrationLabel strips the .sql extension from a file name to form the human-readable name.
function migrationLabel(fileName: string): string {
  return fileName.replace(/\.sql$/, "");
}
