// Views (spec/design/views.md): the planning-trace shapes and the pure text helpers — `*` freezing
// (§5), the persisted dependency list (§6), the RENAME / DROP COLUMN token rewrites (§7), and the
// output-name rules (§1). The Engine methods that plan a view body (CREATE / DROP VIEW, reading a
// view as the equivalent derived table §4, re-validation after ALTER TABLE §8) live in executor.ts.
// Mirrors impl/rust/src/executor/views.rs and impl/go views.go.
//
// A view is stored as canonical token text and planned again at every use. The trace is how the
// engine learns what that text binds to: while a view body is planned at CREATE VIEW, at
// re-validation, or before a rewrite, the planner records into Engine.viewTrace
//   - each FROM item that binds a catalog table or view, keyed by the position of its table-name
//     token (TableRefPos);
//   - each base-table column a name in the body binds to (bare, qualified, correlated, USING /
//     NATURAL on both sides, and `*` expansions);
//   - each written `*` / `rel.*` select item with the columns it expanded to.
//
// Positions are token indices into lex(definition), so every edit is a token edit on the stored text
// followed by the canonical re-render (format.md *Check-expression text*). A nested view reference
// records only the view itself: its own body is planned with the trace suspended.

import type { TableRef, TableRefPos } from "./ast.ts";
import type { Table, ViewDef, ViewDep, ViewDepKind } from "./catalog.ts";
import { type EngineError, engineError } from "./errors.ts";
import { lex } from "./lexer.ts";
import { renderTokens } from "./parser.ts";
import type { Token } from "./token.ts";
import { compareTextC } from "./value.ts";

// TraceItem is one traced FROM item (views.md §7): what it binds and where it is written.
export type TraceItem = { kind: ViewDepKind; name: string; pos: TableRefPos };

// TraceStar is one written `*` / `rel.*` and the columns it expanded to (views.md §5): each column
// with the label it renders under (null = a bare `"column"`).
export type TraceStar = { qualified: boolean; cols: [string | null, string][] };

// ViewTrace is what one traced planning pass of a view body binds (views.md §5/§6/§7).
export class ViewTrace {
  // The FROM items that bind a catalog table or view, keyed by the table-name token position. Keyed
  // (not appended) because a pushed-down derived body is planned twice.
  items: Map<number, TraceItem> = new Map();
  // The built-in jed_ catalog relations the body names (privilege-checked at CREATE, §9).
  catalogRels: Set<string> = new Set();
  // Every referenced base-table column: table name → column ordinals.
  columns: Map<string, Set<number>> = new Map();
  // Each written star's expansion, keyed by its token position.
  stars: Map<number, TraceStar> = new Map();

  addColumn(table: string, ordinal: number): void {
    let set = this.columns.get(table);
    if (set === undefined) {
      set = new Set();
      this.columns.set(table, set);
    }
    set.add(ordinal);
  }

  // itemsInOrder is the traced FROM items in ascending token-position order.
  itemsInOrder(): TraceItem[] {
    return [...this.items.keys()].sort((a, b) => a - b).map((k) => this.items.get(k)!);
  }
}

// viewNotSupported is the 0A000 every view-only narrowing raises.
export function viewNotSupported(msg: string): EngineError {
  return engineError("feature_not_supported", msg);
}

// renamedBaseTable is a copy of a base table (or CTE relation) relabeled by a FROM item's column
// alias list (grammar.md §15): the first list.length columns take the new names, the rest keep
// theirs; a longer list is 42P10. The copy keeps the table name, so the relation still reads the
// table's own store — the rename is a pure resolution-time relabeling.
export function renamedBaseTable(table: Table, tref: TableRef, list: string[]): Table {
  const label = tref.alias ?? tref.name;
  if (list.length > table.columns.length) {
    throw engineError(
      "invalid_column_reference",
      `table "${label}" has ${table.columns.length} columns available but ${list.length} columns specified`,
    );
  }
  return {
    ...table,
    columns: table.columns.map((c, i) => (i < list.length ? { ...c, name: list[i]! } : c)),
  };
}

// viewOutputNames is the view's output column names (views.md §1): the column list applied over the
// body's names left to right. A longer list is 42601; a duplicate final name is 42701; no column is
// 0A000.
export function viewOutputNames(bodyNames: string[], list: string[] | null): string[] {
  const names = bodyNames.slice();
  if (list !== null) {
    if (list.length > names.length) {
      throw engineError("syntax_error", "CREATE VIEW specifies more column names than columns");
    }
    list.forEach((n, i) => {
      names[i] = n;
    });
  }
  for (let i = 0; i < names.length; i++) {
    if (names.slice(0, i).includes(names[i]!)) {
      throw engineError("duplicate_column", `column "${names[i]}" specified more than once`);
    }
  }
  if (names.length === 0) throw viewNotSupported("a view must have at least one column");
  return names;
}

// quoted is the quoted-identifier token every engine-substituted name is written as (views.md §5/§7).
export function quoted(name: string): Token {
  return { kind: "quotedIdent", str: name };
}

const AS_TOKEN: Token = { kind: "word", word: "as" };

// freezeStars replaces every traced written star in the definition's tokens by its expansion
// (views.md §5): a labeled column as `"label" . "column"`, an unlabeled one as `"column"`,
// comma-separated.
export function freezeStars(tokens: Token[], trace: ViewTrace): string {
  const out: Token[] = [];
  let i = 0;
  while (i < tokens.length) {
    if (tokens[i]!.kind === "eof") break;
    const star = trace.stars.get(i);
    if (star === undefined) {
      out.push(tokens[i]!);
      i++;
      continue;
    }
    star.cols.forEach(([label, column], k) => {
      if (k > 0) out.push({ kind: "comma" });
      if (label !== null) {
        out.push(quoted(label));
        out.push({ kind: "dot" });
      }
      out.push(quoted(column));
    });
    i += star.qualified ? 3 : 1;
  }
  return renderTokens(out);
}

// sortedDeps returns deps in ascending name byte order, each table's columns in byte order.
export function sortDeps(deps: ViewDep[]): ViewDep[] {
  for (const d of deps) d.columns.sort(compareTextC);
  return deps.sort((a, b) => compareTextC(a.name, b.name));
}

// depsEqual reports whether two dependency lists are identical.
export function depsEqual(a: ViewDep[], b: ViewDep[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    const x = a[i]!;
    const y = b[i]!;
    if (x.kind !== y.kind || x.name !== y.name || x.columns.length !== y.columns.length) {
      return false;
    }
    for (let k = 0; k < x.columns.length; k++) if (x.columns[k] !== y.columns[k]) return false;
  }
  return true;
}

// viewsInDependencyOrder orders views so each follows every view it reads; ties in name byte order
// (views.md §11 — the CLI dump order). `sorted` is the views in name byte order.
export function viewsInDependencyOrder(sorted: ViewDef[]): ViewDef[] {
  const pending = sorted.slice();
  const out: ViewDef[] = [];
  while (pending.length > 0) {
    const ready = pending.findIndex((v) =>
      v.deps.every((d) => d.kind !== "view" || out.some((done) => done.name === d.name)),
    );
    if (ready < 0) throw new Error("view dependencies are acyclic");
    out.push(pending.splice(ready, 1)[0]!);
  }
  return out;
}

// ---------------------------------------------------------------------------------------------
// ALTER TABLE: rewrites (views.md §7) and re-validation (§8).
// ---------------------------------------------------------------------------------------------

// ViewAlterChange is what an ALTER TABLE statement changed that the dependent views' text must
// follow (§7): RENAME TO, RENAME COLUMN of the column at `ordinal`, or any other action with the
// original ordinals of the columns it dropped (possibly none).
export type ViewAlterChange =
  | { kind: "renameTable"; newName: string }
  | { kind: "renameColumn"; ordinal: number; newName: string }
  | { kind: "other"; dropped: number[] };

// ViewAlterPlan is the dependent views of a table captured before an ALTER TABLE runs (§7/§8): each
// view with the FROM items that bind the table, as traced against the pre-statement catalog, and the
// table's pre-statement name and column names.
export type ViewAlterPlan = {
  table: string;
  columns: string[];
  views: { view: ViewDef; items: TraceItem[] }[];
};

// viewUsing is the first dependent view (name byte order) that references the table's column at the
// pre-statement ordinal `ordinal` (§6), or null.
export function viewUsing(plan: ViewAlterPlan, ordinal: number): string | null {
  const column = plan.columns[ordinal]!;
  for (const { view } of plan.views) {
    if (
      view.deps.some(
        (d) => d.kind === "table" && d.name === plan.table && d.columns.includes(column),
      )
    ) {
      return view.name;
    }
  }
  return null;
}

// alterBreaksView is the 0A000 an ALTER TABLE raises when a dependent view would not survive it (§8).
export function alterBreaksView(table: string, view: string): EngineError {
  return viewNotSupported(
    `cannot alter table ${table} because it would change the definition of view ${view}`,
  );
}

// renamedDeps is a view's dependency list after the statement's own renames (§8): the table's entry
// follows a RENAME TO, its column entry follows a RENAME COLUMN, and the list is re-sorted.
export function renamedDeps(
  deps: ViewDep[],
  plan: ViewAlterPlan,
  change: ViewAlterChange,
): ViewDep[] {
  const out: ViewDep[] = deps.map((d) => ({
    kind: d.kind,
    name: d.name,
    columns: d.columns.slice(),
  }));
  for (const dep of out) {
    if (dep.kind !== "table" || dep.name !== plan.table) continue;
    if (change.kind === "renameTable") {
      dep.name = change.newName;
    } else if (change.kind === "renameColumn") {
      const old = plan.columns[change.ordinal]!;
      dep.columns = dep.columns.map((c) => (c === old ? change.newName : c));
      dep.columns.sort(compareTextC);
    }
  }
  out.sort((a, b) => compareTextC(a.name, b.name));
  return out;
}

// rewriteViewText applies the §7 token edits to a stored definition for each FROM item in `items`
// (all of which bind the altered table) and re-renders it canonically. Every inserted or substituted
// name is a quoted identifier and an inserted keyword is the bare word `as`; untouched tokens keep
// their spelling.
export function rewriteViewText(
  definition: string,
  items: TraceItem[],
  plan: ViewAlterPlan,
  change: ViewAlterChange,
): string {
  const tokens = lex(definition);
  const edits = new TokenEdits();
  for (const item of items) {
    const pos = item.pos;
    if (change.kind === "renameTable") {
      edits.replace(pos.name, [quoted(change.newName)]);
      if (pos.alias === null) edits.insertAfter(pos.name, [AS_TOKEN, quoted(plan.table)]);
    } else if (change.kind === "renameColumn") {
      // The names this item exposed for positions 0..=k before the statement.
      const list = pos.list !== null ? listEntries(tokens, pos.list[0], pos.list[1]) : [];
      if (list.length > change.ordinal) continue;
      const exposed = (j: number): string => list[j] ?? plan.columns[j]!;
      const tail: Token[] = [];
      for (let j = list.length; j <= change.ordinal; j++) {
        if (j > 0) tail.push({ kind: "comma" });
        tail.push(quoted(exposed(j)));
      }
      if (pos.alias === null) {
        edits.insertAfter(pos.name, [
          AS_TOKEN,
          quoted(plan.table),
          { kind: "lparen" },
          ...tail,
          { kind: "rparen" },
        ]);
      } else if (pos.list === null) {
        edits.insertAfter(pos.alias, [{ kind: "lparen" }, ...tail, { kind: "rparen" }]);
      } else {
        edits.insertAfter(pos.list[1] - 1, tail);
      }
    } else {
      if (pos.list === null) continue;
      const [open, close] = pos.list;
      const n = (close - open) / 2;
      const gone = change.dropped.filter((j) => j < n);
      if (gone.length === 0) continue;
      if (gone.length === n) {
        // The whole list goes, with its parentheses.
        for (let t = open; t <= close; t++) edits.delete(t);
        continue;
      }
      // Remove each dropped entry with one separating comma: the comma before it, or — for an
      // entry before the first kept one — the comma after it.
      let keptFirst = 0;
      while (gone.includes(keptFirst)) keptFirst++;
      for (const j of gone) {
        const entry = open + 1 + 2 * j;
        edits.delete(entry);
        edits.delete(j < keptFirst ? entry + 1 : entry - 1);
      }
    }
  }
  return edits.apply(tokens);
}

// listEntries is the identifiers of a column alias list whose `(` / `)` are at open / close.
function listEntries(tokens: Token[], open: number, close: number): string[] {
  const out: string[] = [];
  for (let i = open + 1; i < close; i += 2) {
    const t = tokens[i]!;
    if (t.kind === "word") out.push(t.word!);
    else if (t.kind === "quotedIdent") out.push(t.str!);
  }
  return out;
}

// TokenEdits is a set of token edits keyed by token position, applied in one pass so earlier edits
// never shift later positions.
class TokenEdits {
  private replaced: Map<number, Token[]> = new Map();
  private inserted: Map<number, Token[]> = new Map();
  private deleted: Set<number> = new Set();

  replace(pos: number, with_: Token[]): void {
    this.replaced.set(pos, with_);
  }

  insertAfter(pos: number, tokens: Token[]): void {
    const cur = this.inserted.get(pos);
    if (cur === undefined) this.inserted.set(pos, tokens.slice());
    else cur.push(...tokens);
  }

  delete(pos: number): void {
    this.deleted.add(pos);
  }

  apply(tokens: Token[]): string {
    const out: Token[] = [];
    for (let i = 0; i < tokens.length; i++) {
      const t = tokens[i]!;
      if (t.kind === "eof") break;
      if (!this.deleted.has(i)) {
        const r = this.replaced.get(i);
        if (r !== undefined) out.push(...r);
        else out.push(t);
      }
      const ins = this.inserted.get(i);
      if (ins !== undefined) out.push(...ins);
    }
    return renderTokens(out);
  }
}
