// Parameter-sensitive ("custom") planning (spec/design/estimator.md §6.1).
//
// Planning runs inside a frame that says what the estimator may know about each $N:
//
// - probe: the values are not bound yet. A parameter is a plan-time literal of unknown value: the
//   estimator follows its literal path, and the first read of a parameter's VALUE records that the
//   statement is parameter-sensitive and takes the generic (unbound) estimate. A NULL test of a
//   parameter answers "not NULL" and records that the plan assumed so.
// - custom: the bound values, as the constants a literal of each parameter's resolved type would
//   resolve to. Every estimator read sees the value, so the plan is the custom plan.
//
// A probe plan that recorded nothing never consulted a value, so it is the custom plan for every
// non-NULL binding (and for NULL ones too when no NULL test fired). Each plan→bind boundary plans in
// a probe frame, binds, and plans again in a custom frame only when the probe was not value-blind.
// The core is single-threaded and planning is synchronous, so the frames are one module-level stack,
// strictly nested.

import type { ScalarType } from "./types.ts";
import type { Value } from "./value.ts";
import { render } from "./value.ts";

interface Frame {
  custom: boolean;
  values: Value[];
  sensitive: boolean;
  nullRead: boolean;
  // every type a $N was given while probing (the EXPLAIN typing pass reads it)
  types: (ScalarType | null)[];
  // a restarted statement adopts this custom frame at its own begin instead of probing again
  adopt: boolean;
}

const frames: Frame[] = [];

function top(): Frame | undefined {
  return frames[frames.length - 1];
}

// paramValue is bound parameter i's value for the estimator, or null when it is unavailable (a probe
// frame — which records the statement as parameter-sensitive — or no frame).
export function paramValue(i: number): Value | null {
  const f = top();
  if (f === undefined) return null;
  if (!f.custom) {
    f.sensitive = true;
    return null;
  }
  return f.values[i] ?? null;
}

// paramIsNull reports whether bound parameter i is NULL. An unbound parameter is assumed non-NULL; a
// probe frame records the assumption so a NULL binding is planned again with its value.
export function paramIsNull(i: number): boolean {
  const f = top();
  if (f === undefined) return false;
  if (!f.custom) {
    f.nullRead = true;
    return false;
  }
  return f.values[i]?.kind === "null";
}

// noteParamType records the type a probe frame's resolution gave $(idx0+1) (ParamTypes.note, after
// its own unification; null = no context here, which still makes the slot exist).
export function noteParamType(idx0: number, ty: ScalarType | null): void {
  const f = top();
  if (f === undefined || f.custom) return;
  while (idx0 >= f.types.length) f.types.push(null);
  if (ty !== null) f.types[idx0] = ty;
}

function hasNull(values: Value[]): boolean {
  return values.some((v) => v.kind === "null");
}

// Planning is one plan→bind boundary's frame; end() (in a finally) pops the frame it pushed.
export class Planning {
  private frame: Frame;
  private pushed: boolean;

  private constructor(frame: Frame, pushed: boolean) {
    this.frame = frame;
    this.pushed = pushed;
  }

  // begin opens a boundary: adopt a restart's pending custom frame, else push a probe frame.
  static begin(): Planning {
    const t = top();
    if (t?.custom && t.adopt) {
      t.adopt = false;
      return new Planning(t, false);
    }
    const f: Frame = {
      custom: false,
      values: [],
      sensitive: false,
      nullRead: false,
      types: [],
      adopt: false,
    };
    frames.push(f);
    return new Planning(f, true);
  }

  // probing: the boundary is still probing (it did not adopt a restart's frame).
  probing(): boolean {
    return this.pushed && !this.frame.custom;
  }

  // probeFacts: the probe's two observations — read a value, assumed a parameter non-NULL — that a
  // prepared statement's cache entry keeps.
  probeFacts(): { sensitive: boolean; nullRead: boolean } {
    if (!this.probing()) return { sensitive: false, nullRead: false };
    return { sensitive: this.frame.sensitive, nullRead: this.frame.nullRead };
  }

  // probeTypes: the parameter types the probe saw.
  probeTypes(): (ScalarType | null)[] {
    return this.probing() ? [...this.frame.types] : [];
  }

  // needsCustom: the probe consulted a value — it read one, or it assumed a parameter non-NULL that
  // bound binds to NULL.
  needsCustom(bound: Value[]): boolean {
    if (!this.probing()) return false;
    return this.frame.sensitive || (this.frame.nullRead && hasNull(bound));
  }

  // bind switches the boundary to a custom frame over bound, returning whether the probe plan must
  // be planned again (needsCustom). Planning after the call sees the values.
  bind(bound: Value[]): boolean {
    const again = this.needsCustom(bound);
    if (this.pushed) {
      this.frame.custom = true;
      this.frame.values = bound;
    }
    return again;
  }

  end(): void {
    if (!this.pushed) return;
    this.pushed = false;
    if (frames[frames.length - 1] !== this.frame)
      throw new Error("plan frames are strictly nested");
    frames.pop();
  }
}

function withFrame<T>(bound: Value[], adopt: boolean, f: () => T): T {
  const frame: Frame = {
    custom: true,
    values: bound,
    sensitive: false,
    nullRead: false,
    types: [],
    adopt,
  };
  frames.push(frame);
  try {
    return f();
  } finally {
    frames.pop();
  }
}

// restartPlanning runs f (a restarted statement) under a custom frame over bound that its own
// Planning.begin adopts.
export function restartPlanning<T>(bound: Value[], f: () => T): T {
  return withFrame(bound, true, f);
}

// withPlanValues runs f with the bound values visible to the estimator (planning after binding with
// no probe of its own — a prepared statement's cached types).
export function withPlanValues<T>(bound: Value[], f: () => T): T {
  return withFrame(bound, false, f);
}

// PLAN_MEMO_CAPACITY bounds how many custom plans a parameter-sensitive prepared statement remembers
// (estimator.md §6.1: any bounded policy; the memo is invisible).
export const PLAN_MEMO_CAPACITY = 8;

// planMemoKey is the exact identity of a binding for the custom-plan memo: each value's kind and its
// exact rendering (a float's shortest round-trip text with its sign, a decimal's scale).
export function planMemoKey(bound: Value[]): string {
  return bound
    .map((v) => {
      const text =
        v.kind === "f32" || v.kind === "f64"
          ? Object.is(v.value, -0)
            ? "-0"
            : String(v.value)
          : render(v);
      return `${v.kind}:${text.length}:${text}`;
    })
    .join("|");
}
