// Host extensions (spec/design/extensibility.md §4.2 / §5.1 / §7) — the injection seam a host
// application uses to register its own SCALAR FUNCTIONS OVER EXISTING TYPES. This is delivery step 3
// (extensibility.md §14): runtime host functions, resolved + evaluated through a registry the host
// supplies at open/create and the engine freezes for the handle's lifetime. Deliberately narrow
// (mirrors impl/rust/src/extension.rs and impl/go/extension.go):
//
//   - Functions only (no host types, no format bump).
//   - Ephemeral — reachable from ad-hoc queries while registered; NOT persisted (no DDL, no stored
//     index expression). The catalog-bound, versioned form is step 4.
//   - Strict — a NULL argument short-circuits to NULL before the kernel runs; the kernel never sees
//     a NULL.
//   - Exact scalar signatures — (name, ScalarType[]) → ScalarType, matched by equality (no implicit
//     promotion). A built-in overload always wins over a host one (§4.2).
//   - Single-row kernels ("batch-of-one"); the vectorized ABI is a follow-on.
//
// `volatility` and `crossCore` are RECORDED forward-compat (only "immutable" will later admit
// constant-folding / index-backing; `crossCore` governs the §10 determinism ledger — jed has no
// runtime taint yet, for floats or anything). `cost` IS enforced: the declared static weight
// (cost.md §6 design (a)) is charged per call.

import { engineError } from "./errors.ts";
import { ALL_SCALAR_TYPES, type ScalarType } from "./types.ts";
import type { Value } from "./value.ts";

// A host function's planning volatility (PostgreSQL's notion). Recorded on registration; only
// "immutable" will later admit constant-folding / an index-backing expression. This slice folds no
// host function regardless.
export type Volatility = "immutable" | "stable" | "volatile";

// A host scalar-function kernel: maps evaluated argument values to a result value, or throws an
// EngineError. STRICT — never invoked with a NULL argument (the engine short-circuits NULL→NULL).
export type HostKernel = (args: Value[]) => Value;

// The spec a host passes to ExtensionRegistry.registerFunction. `argTypes`/`result` are canonical
// ScalarType names ("i64", "text", …). Optional fields default to safe values (Volatile, not
// cross-core, unit cost) — matching Rust/Go so a function registered identically on every core
// accrues the same cost.
export interface HostFunctionSpec {
  name: string;
  argTypes: ScalarType[];
  result: ScalarType;
  kernel: HostKernel;
  volatility?: Volatility;
  crossCore?: boolean;
  cost?: bigint;
}

// A registered host function (the internal, defaults-resolved form).
interface HostFuncEntry {
  name: string;
  argTypes: ScalarType[];
  result: ScalarType;
  volatility: Volatility;
  crossCore: boolean;
  cost: bigint;
  kernel: HostKernel;
}

// The immutable set of host extensions supplied at open/create and FROZEN for the database handle's
// lifetime (extensibility.md §7). This slice holds host scalar functions over existing types only.
// Shared (by reference) across every session minted from the handle; a streaming cursor's frozen
// engine shares the same SessionState reference and so sees the same functions.
export class ExtensionRegistry {
  private readonly functions: HostFuncEntry[] = [];

  // Register a host scalar function. Throws on a negative cost (22023), an unknown argument/result
  // type (42704), or a second function with an identical (name, argTypes) signature (42723 —
  // signature-level, not name-level: a host may overload a name across argument types, §4.2). A
  // signature that shadows a built-in is accepted but never reached (built-ins win).
  registerFunction(spec: HostFunctionSpec): void {
    const name = spec.name.toLowerCase();
    const cost = spec.cost ?? 1n;
    if (cost < 0n)
      throw engineError(
        "invalid_parameter_value",
        `host function ${name}: cost must be non-negative`,
      );
    for (const t of spec.argTypes)
      if (!isScalarType(t))
        throw engineError("undefined_object", `host function ${name}: unknown argument type ${t}`);
    if (!isScalarType(spec.result))
      throw engineError(
        "undefined_object",
        `host function ${name}: unknown result type ${spec.result}`,
      );
    for (const g of this.functions)
      if (g.name === name && sameHostSig(g.argTypes, spec.argTypes))
        throw engineError(
          "duplicate_function",
          `host function ${name} already registered with this signature`,
        );
    this.functions.push({
      name,
      argTypes: [...spec.argTypes],
      result: spec.result,
      volatility: spec.volatility ?? "volatile",
      crossCore: spec.crossCore ?? false,
      cost,
      kernel: spec.kernel,
    });
  }

  // Whether any registered host function has this (lowercased) name — the resolve-time routing gate.
  hasFunction(name: string): boolean {
    return this.functions.some((f) => f.name === name);
  }

  // Resolve (name, arg types) to a host-function id (a stable index) by exact scalar signature.
  // null ⇒ no host overload (the caller falls through to 42883).
  resolveHost(name: string, argTypes: ScalarType[]): number | null {
    const i = this.functions.findIndex((f) => f.name === name && sameHostSig(f.argTypes, argTypes));
    return i < 0 ? null : i;
  }

  // The registered function at `id` (an index from resolveHost).
  functionAt(id: number): HostFuncEntry {
    const f = this.functions[id];
    if (!f) throw new Error(`host function id ${id} out of range`);
    return f;
  }
}

function isScalarType(t: string): t is ScalarType {
  return (ALL_SCALAR_TYPES as readonly string[]).includes(t);
}

function sameHostSig(a: readonly ScalarType[], b: readonly ScalarType[]): boolean {
  return a.length === b.length && a.every((t, i) => t === b[i]);
}

// Whether `v` is a valid value for declared scalar result type `ty` — used to check a host kernel's
// return against its declared result type, so a misbehaving host function cannot leak a wrong-typed
// value into jed's strict type system (defense of jed's own invariants — the host owns its
// consequences, CLAUDE.md §13, but jed's codecs/comparators must never see a type violation). NULL is
// always valid (a strict function may still return NULL for non-null args).
export function valueMatchesResult(v: Value, ty: ScalarType): boolean {
  switch (v.kind) {
    case "null":
      return true;
    case "int":
      return ty === "i16" || ty === "i32" || ty === "i64";
    case "bool":
      return ty === "boolean";
    case "f32":
      return ty === "f32";
    case "f64":
      return ty === "f64";
    case "text":
      return ty === "text";
    case "decimal":
      return ty === "decimal";
    case "bytea":
      return ty === "bytea";
    case "uuid":
      return ty === "uuid";
    case "timestamp":
      return ty === "timestamp";
    case "timestamptz":
      return ty === "timestamptz";
    case "date":
      return ty === "date";
    case "interval":
      return ty === "interval";
    case "json":
      return ty === "json";
    case "jsonb":
      return ty === "jsonb";
    case "jsonpath":
      return ty === "jsonpath";
    default:
      // composite / array / range / unfetched: a host scalar function declares a scalar result.
      return false;
  }
}
