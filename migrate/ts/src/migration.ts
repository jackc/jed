// The migration file format (design.md §4): the up/down split and the non-empty-up rule.

import { splitStatements } from "../../../impl/ts/src/lib.ts";
import { LoadError } from "./errors.ts";

// SEPARATOR is the magic line that splits a migration file's up half from its down half
// (design.md §4). Kept verbatim from tern; it is itself a valid jed "--" line comment, so a
// file is inert if ever fed straight to the engine.
export const SEPARATOR = "---- create above / drop below ----";

// Migration is one loaded migration (design.md §4/§6). `sequence` is 1-based; `name` is the
// free-form label from the filename. `up` is the forward SQL (never empty). `down` is null
// exactly when the migration is irreversible (the file had no separator).
export type Migration = {
  sequence: number;
  name: string;
  up: string;
  down: string | null;
};

// isIrreversible reports whether a migration has no down half.
export function isIrreversible(m: Migration): boolean {
  return m.down === null;
}

// parseMigration splits a file's raw `contents` into a Migration (design.md §4). The file is
// split on the separator line; text before it is the up half, text after it is the down half.
// A file with no separator is up-only (irreversible). The up half must be non-empty (only
// whitespace/comments is a load-time error).
export function parseMigration(sequence: number, name: string, contents: string): Migration {
  const { up, down } = splitHalves(name, contents);
  if (!hasSql(up)) {
    throw new LoadError(`migration "${name}": no SQL in forward migration step`);
  }
  return { sequence, name, up, down };
}

// splitHalves splits `contents` on the SEPARATOR line. A line is a separator iff its trimmed
// content equals SEPARATOR, so trailing whitespace / a Windows "\r" is tolerated. At most one
// separator is allowed (design.md §7): a second separator line is a load-time error rather
// than silently folding into the down half. `down` is null when there is no separator.
function splitHalves(name: string, contents: string): { up: string; down: string | null } {
  const lines = contents.split("\n");
  let sep = -1;
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].trim() === SEPARATOR) {
      if (sep !== -1) {
        throw new LoadError(`migration "${name}": more than one separator line (${SEPARATOR})`);
      }
      sep = i;
    }
  }
  if (sep === -1) {
    return { up: contents, down: null };
  }
  return {
    up: lines.slice(0, sep).join("\n"),
    down: lines.slice(sep + 1).join("\n"),
  };
}

// hasSql reports whether `text` contains any SQL beyond whitespace and comments — the check
// that a migration half is non-empty. Reuses the engine's lexer-aware splitter, which skips
// comment-only / blank spans, so a half of only comments yields zero statements.
function hasSql(text: string): boolean {
  for (const _ of splitStatements(text)) {
    return true;
  }
  return false;
}
