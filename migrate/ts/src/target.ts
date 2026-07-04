// Resolving tern-style destination specs to absolute target versions (design.md §6/§9).

import { BadVersionError, LoadError } from "./errors.ts";

// resolveTargets resolves a tern-style destination `spec` into the ordered list of absolute
// target versions to migrate to (design.md §6/§9). `current` is the version presently
// recorded; `n` is the highest available sequence. The grammar:
//
//   "last" or ""  → migrate to N (the default)
//   "<integer>"   → migrate to that absolute version
//   "+N"          → migrate up N steps   (current + N)
//   "-N"          → migrate down N steps (current - N)
//   "-+N"         → redo the last N: down N, then back up N ([current-N, current])
//
// Every resolved target is range-checked against 0 … N. Relative-grammar resolution is the
// caller's concern (design.md §9 — typically a CLI); the library's migrateTo takes only an
// absolute target.
export function resolveTargets(spec: string, current: number, n: number): number[] {
  spec = spec.trim();
  if (spec === "" || spec === "last") return [n];

  // Redo: "-+N" (down N, then back up N). Checked before the "-N" case.
  if (spec.startsWith("-+")) {
    const steps = parseSteps(spec.slice(2), spec);
    const down = current - steps;
    checkRange(down, n);
    return [down, current];
  }

  // Relative up/down: "+N" / "-N".
  if (spec.startsWith("+")) {
    const target = current + parseSteps(spec.slice(1), spec);
    checkRange(target, n);
    return [target];
  }
  if (spec.startsWith("-")) {
    const target = current - parseSteps(spec.slice(1), spec);
    checkRange(target, n);
    return [target];
  }

  // Absolute integer.
  const target = parseSteps(spec, spec);
  checkRange(target, n);
  return [target];
}

// parseSteps parses a non-negative decimal integer, throwing a LoadError naming the whole spec
// on anything else (an empty string, a sign, a non-digit).
function parseSteps(text: string, spec: string): number {
  if (text.length === 0 || !/^\d+$/.test(text)) {
    throw new LoadError(`bad destination "${spec}": expected +N, -N, -+N, an integer, or last`);
  }
  return Number.parseInt(text, 10);
}

function checkRange(target: number, n: number): void {
  if (target < 0 || target > n) {
    throw new BadVersionError(target, n, "target");
  }
}
