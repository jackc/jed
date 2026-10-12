// Error Hint text (spec/design/error-fields.md §11): PostgreSQL's misspelled-column suggestion
// (errorMissingColumn in parse_relation.c) and its table-alias suggestion. Pure functions over the
// names in scope; the Scope (scope.ts) gathers the candidates in the §11.1 walk order.

import { utf8Length } from "./memsize.ts";

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
