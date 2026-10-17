// GIN query trees and the jsonb_ops jsonpath term extraction (spec/design/gin.md §2.2). Mirrors
// impl/rust/src/gin_jsonpath.rs.
//
// A GIN query is a monotone AND/OR tree over terms (GinTree): a row is a candidate when the tree is
// TRUE with each term read as "the row carries this term". The ordinary strategies are one level deep
// (mode ALL is an AND of terms, mode ANY an OR); the jsonpath strategies @? / @@ extract a deeper tree
// from the path, PostgreSQL's extract_jsp_query (jsonb_gin.c) for jsonb_ops, term for term.

import { ginJsonbKeyTerm, ginJsonbScalarTerm } from "./gin_jsonb.ts";
import type { JsonNode } from "./json.ts";
import type { JsonPath, PathExpr, Pred } from "./jsonpath.ts";

// GinTree is a normalized GIN query tree (gin.md §2.2). An and / or node holds its distinct leaf
// terms in ascending byte order and its non-leaf children in extraction order; nested nodes of the
// same kind are spliced into their parent, and a node with one child is that child.
export type GinTree =
  | { kind: "term"; term: Uint8Array }
  | { kind: "and" | "or"; leaves: Uint8Array[]; rest: GinTree[] };

function cmpBytes(a: Uint8Array, b: Uint8Array): number {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) {
    if (a[i]! !== b[i]!) return a[i]! - b[i]!;
  }
  return a.length - b.length;
}

function normalize(kind: "and" | "or", nodes: GinTree[]): GinTree {
  const leaves: Uint8Array[] = [];
  const rest: GinTree[] = [];
  for (const n of nodes) {
    if (n.kind === "term") leaves.push(n.term);
    else if (n.kind === kind) {
      leaves.push(...n.leaves);
      rest.push(...n.rest);
    } else rest.push(n);
  }
  leaves.sort(cmpBytes);
  const distinct = leaves.filter((t, i) => i === 0 || cmpBytes(leaves[i - 1]!, t) !== 0);
  if (distinct.length === 1 && rest.length === 0) return { kind: "term", term: distinct[0]! };
  if (distinct.length === 0 && rest.length === 1) return rest[0]!;
  return { kind, leaves: distinct, rest };
}

// ginTreeFromTerms is a strategy's flat term set as a tree: mode ANY an or, mode ALL an and.
export function ginTreeFromTerms(any: boolean, terms: Uint8Array[]): GinTree {
  return normalize(
    any ? "or" : "and",
    terms.map((term) => ({ kind: "term", term })),
  );
}

// ginTreeMapTerms is the tree with every leaf term mapped through f (a multi-column index's column
// tag, gin.md §4.4); the mapping preserves byte order and distinctness.
export function ginTreeMapTerms(t: GinTree, f: (term: Uint8Array) => Uint8Array): GinTree {
  if (t.kind === "term") return { kind: "term", term: f(t.term) };
  return { kind: t.kind, leaves: t.leaves.map(f), rest: t.rest.map((c) => ginTreeMapTerms(c, f)) };
}

// One accessor of the path being walked (PostgreSQL's JsonPathGinPathItem): a .key carries its KEY
// term; the wildcards and subscripts carry none.
type Item =
  | { kind: "key"; term: Uint8Array }
  | { kind: "anyKey" }
  | { kind: "anyArray" }
  | { kind: "indexArray" }
  | { kind: "any" };

// An extracted node before normalization (PostgreSQL's JsonPathGinNode).
type Node = { kind: "entry"; term: Uint8Array } | { kind: "and" | "or"; children: Node[] };

function toTree(n: Node): GinTree {
  if (n.kind === "entry") return { kind: "term", term: n.term };
  return normalize(n.kind, n.children.map(toTree));
}

// jsonPathGinTree is the jsonb_ops query tree of a jsonpath under @? (exists) or @@ (gin.md §2.2), or
// null when nothing is extractable — the full-scan fallback (PostgreSQL's GIN_SEARCH_MODE_ALL).
export function jsonPathGinTree(path: JsonPath, exists: boolean): GinTree | null {
  const lax = !path.strict;
  let node: Node | null = null;
  if (exists && path.body.kind === "path") node = pathExpr(lax, [], path.body.expr, null);
  else if (!exists && path.body.kind === "predicate")
    node = boolExpr(lax, [], path.body.pred, false);
  return node === null ? null : toTree(node);
}

// boolExpr is PostgreSQL's extract_jsp_bool_expr. Under not, an AND with one unextractable side
// keeps the other only when the node is still an AND after De Morgan (PostgreSQL keeps it whenever
// the operator is written &&, which drops rows under !) — a deliberate, sound divergence (gin.md §9).
function boolExpr(lax: boolean, path: Item[], pred: Pred, not: boolean): Node | null {
  switch (pred.kind) {
    case "and":
    case "or": {
      const and = not !== (pred.kind === "and");
      const l = boolExpr(lax, path, pred.a, not);
      const r = boolExpr(lax, path, pred.b, not);
      if (l !== null && r !== null) return { kind: and ? "and" : "or", children: [l, r] };
      if (and) return l ?? r;
      return null;
    }
    case "not":
      return boolExpr(lax, path, pred.p, !not);
    case "exists":
      return not ? null : pathExpr(lax, path, pred.e, null);
    case "compare": {
      if (not || pred.op !== "eq") return null;
      const ls = scalarLiteral(pred.lhs);
      if (ls !== null) return pathExpr(lax, path, pred.rhs, ls);
      const rs = scalarLiteral(pred.rhs);
      if (rs !== null) return pathExpr(lax, path, pred.lhs, rs);
      return null;
    }
    default:
      return null;
  }
}

// scalarLiteral is a comparison operand PostgreSQL's jspIsScalar accepts: a step-less null / boolean /
// number / string literal.
function scalarLiteral(e: PathExpr): JsonNode | null {
  return e.primary.kind === "lit" && e.steps.length === 0 ? e.primary.node : null;
}

// pathExpr is PostgreSQL's extract_jsp_path_expr: the nodes of expr AND-ed, or null when none.
function pathExpr(
  lax: boolean,
  path: Item[],
  expr: PathExpr,
  scalar: JsonNode | null,
): Node | null {
  const nodes = pathNodes(lax, path, expr, scalar);
  if (nodes.length === 0) return null;
  if (nodes.length === 1) return nodes[0]!;
  return { kind: "and", children: nodes };
}

// pathNodes is PostgreSQL's extract_jsp_path_expr_nodes + jsonb_ops__extract_nodes: walk the
// accessors, extracting each filter; an accessor the opclass cannot follow ends the walk with only the
// filter nodes found so far. A complete walk with a scalar adds a KEY entry per .key accessor (last to
// first) and the scalar's entry.
function pathNodes(lax: boolean, outer: Item[], expr: PathExpr, scalar: JsonNode | null): Node[] {
  const nodes: Node[] = [];
  let path: Item[];
  if (expr.primary.kind === "root") path = [];
  else if (expr.primary.kind === "current") path = [...outer];
  else return nodes;
  for (const step of expr.steps) {
    switch (step.kind) {
      case "filter": {
        const n = boolExpr(lax, path, step.pred, false);
        if (n !== null) nodes.push(n);
        continue;
      }
      case "member":
        path.push({ kind: "key", term: ginJsonbKeyTerm(step.key) });
        continue;
      case "wildcardMember":
        path.push({ kind: "anyKey" });
        continue;
      case "wildcardElement":
        path.push({ kind: "anyArray" });
        continue;
      case "subscripts":
        path.push({ kind: "indexArray" });
        continue;
      case "any":
        path.push({ kind: "any" });
        continue;
      default:
        return nodes;
    }
  }
  if (scalar === null) return nodes;
  for (let i = path.length - 1; i >= 0; i--) {
    const item = path[i]!;
    if (item.kind === "key") nodes.push({ kind: "entry", term: item.term });
  }
  if (scalar.kind !== "string") {
    nodes.push({ kind: "entry", term: ginJsonbScalarTerm(scalar, false) });
    return nodes;
  }
  // jsonb_ops indexes a string array element as a KEY, so the scalar may be a key, a value, or
  // either (lax mode unwraps arrays; .** reaches both).
  let maybe = lax;
  let asKey = false;
  const last = path[path.length - 1];
  if (!lax && last !== undefined) {
    if (last.kind === "anyArray" || last.kind === "indexArray") asKey = true;
    else if (last.kind === "any") maybe = true;
  }
  if (maybe)
    nodes.push({
      kind: "or",
      children: [
        { kind: "entry", term: ginJsonbScalarTerm(scalar, true) },
        { kind: "entry", term: ginJsonbScalarTerm(scalar, false) },
      ],
    });
  else nodes.push({ kind: "entry", term: ginJsonbScalarTerm(scalar, asKey) });
  return nodes;
}
