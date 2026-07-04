// Scaffolding a new migration file (design.md §9). Needs no database.

import { existsSync, mkdirSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { LoadError } from "./errors.ts";
import { SEPARATOR } from "./migration.ts";

// The scaffold written by newMigration: an empty up half, the separator, and an empty down
// half (design.md §9). Delete the separator and the down half for an irreversible migration.
const STUB_TEMPLATE = `-- Write your forward (up) migration here.


${SEPARATOR}

-- Write your reverse (down) migration here.
-- Delete this half (and the separator line above) for an irreversible migration.
`;

const FILE_NAME_PATTERN = /^(\d+)_.+\.sql$/;

// newMigration scaffolds the next migration file in `dir` and returns its path (design.md §9).
// The sequence number is the highest existing sequence plus one (or 1 if the directory is
// empty), zero-padded to three digits, with `name` as the label: dir/NNN_<name>.sql. It needs
// no database. The directory is created if it does not exist.
export function newMigration(dir: string, name: string): string {
  if (name === "") {
    throw new LoadError("new migration needs a name");
  }
  mkdirSync(dir, { recursive: true });
  const next = nextSequence(dir);
  const fileName = `${String(next).padStart(3, "0")}_${name}.sql`;
  const path = join(dir, fileName);
  if (existsSync(path)) {
    throw new LoadError(`${path} already exists`);
  }
  writeFileSync(path, STUB_TEMPLATE);
  return path;
}

// nextSequence returns the next sequence number for `dir`: the highest present migration
// sequence plus one (or 1 when none are present). Unlike loading, this needs only the maximum.
function nextSequence(dir: string): number {
  let max = 0;
  for (const name of readdirSync(dir)) {
    const match = FILE_NAME_PATTERN.exec(name);
    if (!match) continue;
    const seq = Number.parseInt(match[1], 10);
    if (seq > max) max = seq;
  }
  return max + 1;
}
