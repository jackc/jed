// Error Hint text (spec/design/error-fields.md §11): PostgreSQL's misspelled-column suggestion
// (errorMissingColumn in parse_relation.c) and its table-alias suggestion. Pure functions over the
// names in scope; the Scope (scope.ts) gathers the candidates in the §11.1 walk order.

import { utf8Length } from "./memsize.ts";

// PostgreSQL 18's fixed hint sentences (spec/design/error-fields.md §11.7–§11.8), byte-for-byte.

// 42883 — no function matches a call's name and argument types (parse_func.c).
export const FUNCTION_NO_MATCH =
  "No function matches the given name and argument types. You might need to add explicit type casts.";
// 42725 — several functions match and none is best (parse_func.c).
export const FUNCTION_AMBIGUOUS =
  "Could not choose a best candidate function. You might need to add explicit type casts.";
// 42883 — no binary operator matches its operand types (parse_oper.c op_error).
export const OPERATOR_NO_MATCH =
  "No operator matches the given name and argument types. You might need to add explicit type casts.";
// 42883 — no prefix operator matches its operand type (op_error, one argument).
export const OPERATOR_NO_MATCH_PREFIX =
  "No operator matches the given name and argument type. You might need to add an explicit type cast.";
// 42725 — several operators match and none is best (op_error).
export const OPERATOR_AMBIGUOUS =
  "Could not choose a best candidate operator. You might need to add explicit type casts.";
// 42883 — a sort key's type has no ordering operator (get_sort_group_operators).
export const NO_ORDERING_OPERATOR = "Use an explicit ordering operator or modify the query.";
// 42804 — a value of the wrong type for its column or default (transformAssignedExpr, cookDefault).
export const ASSIGNMENT_MISMATCH = "You will need to rewrite or cast the expression.";
// 42804 — a recursive CTE column whose recursive term changes its type (parse_cte.c).
export const RECURSIVE_TERM_TYPE = "Cast the output of the non-recursive term to the correct type.";
// 42P21 — two different implicit collations meet where a collation is required at plan time
// (select_common_collation, assign_collations_walker, parse_collate.c).
export const COLLATION_IMPLICIT_MISMATCH =
  "You can choose the collation by applying the COLLATE clause to one or both expressions.";
// 42P21 — a recursive CTE's recursive term changes a column's collation (parse_cte.c).
export const RECURSIVE_TERM_COLLATION =
  "Use the COLLATE clause to set the collation of the non-recursive term.";
// 42P22 — no collation could be determined for a value that needs one (varstr_cmp,
// intorel_startup, …).
export const INDETERMINATE_COLLATION = "Use the COLLATE clause to set the collation explicitly.";
// 2BP01 — a DROP (or ALTER TABLE … DROP) that other objects depend on, where the statement accepts
// CASCADE (reportDependentObjects, dependency.c).
export const DROP_CASCADE = "Use DROP ... CASCADE to drop the dependent objects too.";

// dropOwnerInstead is the 2BP01 hint for dropping an index a constraint owns (findDependentObjects,
// dependency.c).
export function dropOwnerInstead(constraint: string, table: string): string {
  return `You can drop constraint ${constraint} on table ${table} instead.`;
}
// 42703 — the column is in a FROM item to the left of a non-LATERAL subquery (errorMissingColumn).
export const LATERAL_COLUMN = "To reference that column, you must mark this subquery with LATERAL.";
// 42P01 — the table is a FROM item to the left of a non-LATERAL subquery (errorMissingRTE).
export const LATERAL_TABLE = "To reference that table, you must mark this subquery with LATERAL.";
// 42703 — the column's relation is visible only by its qualifier (errorMissingColumn).
export const QUALIFY_COLUMN = "To reference that column, you must use a table-qualified name.";
// 42703 — several unreachable relations have the column (errorMissingColumn).
export const TRY_QUALIFIED = "Try using a table-qualified name.";

// MAX_FUZZY_DISTANCE is PostgreSQL's; the search starts one above it.
const MAX_FUZZY_DISTANCE = 3;

// levenshtein is the edit distance from a to b over characters (code points, so a surrogate pair is
// one character), every edit costing 1. PostgreSQL's bounded varstr_levenshtein_less_equal may stop
// early above its bound, but a distance above the bound is never used, so the full distance gives the
// same outcome (§11.1).
export function levenshtein(a: string, b: string): number {
  const ac = Array.from(a);
  const bc = Array.from(b);
  let prev: number[] = [];
  for (let j = 0; j <= bc.length; j++) prev.push(j);
  let cur: number[] = new Array<number>(bc.length + 1).fill(0);
  for (let i = 0; i < ac.length; i++) {
    cur[0] = i + 1;
    for (let j = 0; j < bc.length; j++) {
      const sub = prev[j]! + (ac[i] === bc[j] ? 0 : 1);
      cur[j + 1] = Math.min(sub, prev[j + 1]! + 1, cur[j]! + 1);
    }
    const t = prev;
    prev = cur;
    cur = t;
  }
  return prev[bc.length]!;
}

// FuzzyColumnMatch is the fuzzy-match state of §11.1 step 3, fed candidates in walk order.
export class FuzzyColumnMatch {
  // The written column name `c`.
  private target: string;
  // The written qualifier `q`, or null for a bare reference.
  private qualifier: string | null;
  private distance: number;
  private first: { label: string; column: string } | null;
  private second: { label: string; column: string } | null;
  private exact: boolean;

  constructor(qualifier: string | null, target: string) {
    this.target = target;
    this.qualifier = qualifier;
    this.distance = MAX_FUZZY_DISTANCE + 1;
    this.first = null;
    this.second = null;
    this.exact = false;
  }

  // penalty is the relation penalty for a relation labeled `label` (§11.1 step 2).
  penalty(label: string): number {
    return this.qualifier === null ? 0 : levenshtein(this.qualifier, label);
  }

  // offer offers one candidate column of the relation labeled `label`, whose penalty is `penalty`.
  offer(label: string, penalty: number, column: string): void {
    if (penalty > this.distance) return;
    let d = levenshtein(column, this.target);
    if (penalty === 0 && d === 0) this.exact = true;
    // `len` in bytes, as PostgreSQL's strlen.
    if (d > Math.floor(utf8Length(this.target) / 2)) return;
    d += penalty;
    const candidate = { label, column };
    if (d < this.distance) {
      this.distance = d;
      this.first = candidate;
      this.second = null;
    } else if (d === this.distance) {
      if (this.second !== null) {
        // Three at one distance are too many; keep the distance so only a closer match counts.
        this.first = null;
        this.second = null;
      } else if (this.first !== null) {
        this.second = candidate;
      }
    }
  }

  // hint is the hint text (§11.1 step 5), or undefined — no match, or an exact match (step 4).
  hint(): string | undefined {
    if (this.exact || this.first === null) return undefined;
    const one = `"${this.first.label}.${this.first.column}"`;
    if (this.second === null) return `Perhaps you meant to reference the column ${one}.`;
    const two = `"${this.second.label}.${this.second.column}"`;
    return `Perhaps you meant to reference the column ${one} or the column ${two}.`;
  }
}

// aliasHint is the §11.2 table-alias hint for an aliased relation labeled `label`.
export function aliasHint(label: string): string {
  return `Perhaps you meant to reference the table alias "${label}".`;
}

// unreachableColumnDetail is the 42703 Detail when one relation the reference cannot see has the
// column (errorMissingColumn, spec/design/error-fields.md §11.8).
export function unreachableColumnDetail(column: string, label: string): string {
  return `There is a column named "${column}" in table "${label}", but it cannot be referenced from this part of the query.`;
}

// unreachableColumnsDetail is the 42703 Detail when several relations the reference cannot see have
// the column (errorMissingColumn).
export function unreachableColumnsDetail(column: string): string {
  return `There are columns named "${column}", but they are in tables that cannot be referenced from this part of the query.`;
}

// unreachableTableDetail is the 42P01 Detail when the qualifier names a relation the reference
// cannot see (errorMissingRTE).
export function unreachableTableDetail(label: string): string {
  return `There is an entry for table "${label}", but it cannot be referenced from this part of the query.`;
}
