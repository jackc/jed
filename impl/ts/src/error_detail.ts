import { runSync, type Steps } from "./execution.ts";
// Error `Detail` — PostgreSQL's DETAIL sentence naming the values that broke a constraint
// (spec/design/error-fields.md §9). Built only after the error is decided, from values the statement
// already holds: unmetered, so it can never change the code, the cost, or which error fires. Every
// helper applies the §9.4 privilege gate and returns undefined when the detail is withheld or cannot
// be rendered. Mirrors impl/rust/src/executor/error_detail.rs and impl/go error_detail.go.

import type { Column, ExclusionConstraint, ForeignKey, IndexDef, Table } from "./catalog.ts";
import type { EvalEnv, ResolvedIndex } from "./executor.ts";
import { evalExprSteps, synchronousEvalEnv } from "./eval.ts";
import { resolveUnfetchedSelfSteps } from "./format.ts";
import { quoteIdentifier } from "./parser.ts";
import type { SessionState } from "./session.ts";
import type { Row } from "./storage.ts";
import { renderIn, type Value } from "./value.ts";

// The 23505 endings: a write's duplicate (`already exists.`) and a unique-index build over existing
// duplicates (`is duplicated.` — error-fields.md §9.2).
export const DETAIL_EXISTS = "already exists.";
export const DETAIL_DUPLICATED = "is duplicated.";

// The per-value cap, in UTF-8 bytes, before a rendered value is cut and suffixed `...` (§9.3 — PG's
// ExecBuildSlotValueDescription maxfieldlen).
const DETAIL_VALUE_MAX = 64;

// clip cuts s to at most DETAIL_VALUE_MAX UTF-8 bytes at a character boundary, appending `...` when
// anything was cut (§9.3). JS strings are UTF-16, so the byte length is measured per code point and a
// surrogate pair (a 4-byte character) is never split.
function clip(s: string): string {
  // Every UTF-16 unit encodes to at most 3 UTF-8 bytes, so a short string needs no scan.
  if (s.length * 3 <= DETAIL_VALUE_MAX) return s;
  let bytes = 0;
  let i = 0;
  while (i < s.length) {
    const c = s.codePointAt(i)!;
    const n = c < 0x80 ? 1 : c < 0x800 ? 2 : c < 0x10000 ? 3 : 4;
    if (bytes + n > DETAIL_VALUE_MAX) return s.slice(0, i) + "...";
    bytes += n;
    i += c >= 0x10000 ? 2 : 1;
  }
  return s;
}

// columnLabel is a column label: jed's identifier quoting rule (grammar.md §3).
function columnLabel(columns: Column[], ordinal: number): string {
  return quoteIdentifier(columns[ordinal]!.name);
}

// keyPhrase is `Key (a, b)=(1, x)` — the shared key phrase of the 23505/23503/23P01 forms.
function keyPhrase(labels: string[], values: string[]): string {
  return "Key (" + labels.join(", ") + ")=(" + values.join(", ") + ")";
}

// detailVisible is the §9.4 gate: whether this session may see relation's values.
function detailVisible(session: SessionState, relation: string): boolean {
  const p = session.privileges;
  return p.isPermissive() || p.allowsTable(relation, "select");
}

// detailValue renders one value through the result-cell renderer in the session zone (§9.3): a
// top-level NULL is `null`, and the text is clipped. undefined if a deferred value cannot be resolved
// or the value cannot render (an out-of-range zoned instant) — the detail is then omitted, never wrong.
function* detailValueSteps(session: SessionState, v: Value): Steps<string | undefined> {
  try {
    const r = v.kind === "unfetched" ? yield* resolveUnfetchedSelfSteps(v.ref) : v;
    if (r.kind === "null") return "null";
    if (r.kind === "text") return clip(r.text);
    return clip(renderIn(r, session.timeZone));
  } catch {
    return undefined;
  }
}

function* detailValuesSteps(session: SessionState, values: Value[]): Steps<string[] | undefined> {
  const out: string[] = [];
  for (const v of values) {
    const s = yield* detailValueSteps(session, v);
    if (s === undefined) return undefined;
    out.push(s);
  }
  return out;
}

// columnKeyPhrase is the key phrase over plain-column ordinals of row.
function* columnKeyPhraseSteps(
  session: SessionState,
  columns: Column[],
  ordinals: number[],
  row: Row,
): Steps<string | undefined> {
  const labels = ordinals.map((o) => columnLabel(columns, o));
  const values = yield* detailValuesSteps(
    session,
    ordinals.map((o) => row[o]!),
  );
  return values === undefined ? undefined : keyPhrase(labels, values);
}

// pkDetail — 23505 on the primary key: `Key (id)=(1) already exists.` (or `is duplicated.` from a DDL
// build — pass ending).
export function* pkDetailSteps(
  session: SessionState,
  relation: string,
  columns: Column[],
  pk: number[],
  row: Row,
  ending: string,
): Steps<string | undefined> {
  if (!detailVisible(session, relation)) return undefined;
  const phrase = yield* columnKeyPhraseSteps(session, columns, pk, row);
  return phrase === undefined ? undefined : phrase + " " + ending;
}

// indexDetail — 23505 on a unique index: the index's key labels (a column's quoted name, an
// expression's persisted canonical text) and the row's key values, evaluated afresh (unmetered) for an
// expression key.
export function* indexDetailSteps(
  session: SessionState,
  relation: string,
  columns: Column[],
  def: IndexDef,
  rindex: ResolvedIndex,
  row: Row,
  env: EvalEnv,
  ending: string,
): Steps<string | undefined> {
  if (!detailVisible(session, relation)) return undefined;
  const labels: string[] = [];
  const values: string[] = [];
  for (let i = 0; i < def.keys.length && i < rindex.keys.length; i++) {
    const k = def.keys[i]!;
    const rk = rindex.keys[i]!;
    labels.push(k.kind === "column" ? columnLabel(columns, k.column) : k.exprText);
    let v: Value;
    if (rk.kind === "column") v = row[rk.column]!;
    else {
      try {
        v = yield* evalExprSteps(rk.expr, row, env, session.scratchMeter());
      } catch {
        return undefined;
      }
    }
    const s = yield* detailValueSteps(session, v);
    if (s === undefined) return undefined;
    values.push(s);
  }
  return keyPhrase(labels, values) + " " + ending;
}

// indexDetailByName — 23505 on a unique index found by name on table's catalog entry (the raise sites
// that hold only the resolved index).
export function* indexDetailByNameSteps(
  session: SessionState,
  table: Table,
  rindex: ResolvedIndex,
  row: Row,
  env: EvalEnv,
  ending: string,
): Steps<string | undefined> {
  const def = table.indexes.find((d) => d.name === rindex.name);
  if (def === undefined) return undefined;
  return yield* indexDetailSteps(session, table.name, table.columns, def, rindex, row, env, ending);
}

// failingRowDetail — 23514 / 23502: `Failing row contains (…).` — every column of the candidate row.
export function* failingRowDetailSteps(
  session: SessionState,
  relation: string,
  row: Row,
): Steps<string | undefined> {
  if (!detailVisible(session, relation)) return undefined;
  const values = yield* detailValuesSteps(session, row);
  return values === undefined ? undefined : "Failing row contains (" + values.join(", ") + ").";
}

// fkChildDetail — 23503 child side: `Key (p)=(99) is not present in table "t".` — degraded to the
// value-free `Key is not present in table "t".` without SELECT on the child (§9.4).
export function* fkChildDetailSteps(
  session: SessionState,
  relation: string,
  columns: Column[],
  fk: ForeignKey,
  row: Row,
): Steps<string | undefined> {
  const tail = 'is not present in table "' + fk.refTable + '".';
  if (!detailVisible(session, relation)) return "Key " + tail;
  const phrase = yield* columnKeyPhraseSteps(session, columns, fk.columns, row);
  return phrase === undefined ? undefined : phrase + " " + tail;
}

// fkParentDetail — 23503 parent side: `Key (id)=(1) is still referenced from table "c".` — the
// parent's referenced columns over values, the removed/changed parent tuple in FK slot order;
// value-free without SELECT on the parent (§9.4).
export function* fkParentDetailSteps(
  session: SessionState,
  parent: Table,
  fk: ForeignKey,
  child: string,
  values: Value[],
): Steps<string | undefined> {
  const tail = 'is still referenced from table "' + child + '".';
  if (!detailVisible(session, parent.name)) return "Key " + tail;
  const labels = fk.refColumns.map((o) => columnLabel(parent.columns, o));
  const rendered = yield* detailValuesSteps(session, values);
  return rendered === undefined ? undefined : keyPhrase(labels, rendered) + " " + tail;
}

// exclusionDetail — 23P01: `Key (r)=(new) conflicts with existing key (r)=(other).` (existing false
// gives the DDL build's `conflicts with key`).
export function* exclusionDetailSteps(
  session: SessionState,
  relation: string,
  columns: Column[],
  exc: ExclusionConstraint,
  row: Row,
  other: Row,
  existing: boolean,
): Steps<string | undefined> {
  if (!detailVisible(session, relation)) return undefined;
  const ordinals = exc.elements.map((e) => e.column);
  const fresh = yield* columnKeyPhraseSteps(session, columns, ordinals, row);
  const old = yield* columnKeyPhraseSteps(session, columns, ordinals, other);
  if (fresh === undefined || old === undefined) return undefined;
  const with_ = existing ? "existing key" : "key";
  return fresh + " conflicts with " + with_ + " " + old.slice("Key ".length) + ".";
}

export function indexDetailByName(
  session: SessionState,
  table: Table,
  rindex: ResolvedIndex,
  row: Row,
  env: EvalEnv,
  ending: string,
): string | undefined {
  return runSync(
    indexDetailByNameSteps(session, table, rindex, row, synchronousEvalEnv(env), ending),
  );
}

export function failingRowDetail(
  session: SessionState,
  relation: string,
  row: Row,
): string | undefined {
  return runSync(failingRowDetailSteps(session, relation, row));
}

export function fkParentDetail(
  session: SessionState,
  parent: Table,
  fk: ForeignKey,
  child: string,
  values: Value[],
): string | undefined {
  return runSync(fkParentDetailSteps(session, parent, fk, child, values));
}
