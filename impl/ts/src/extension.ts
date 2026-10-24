// Host extensions (spec/design/extensibility.md §4.2 / §5.1 / §7) — the injection seam a host
// application uses to register its own SCALAR FUNCTIONS OVER EXISTING TYPES. This is delivery step 3
// (extensibility.md §14): runtime host functions, registered on the EnvironmentBuilder
// (spec/design/environment.md §5), frozen in the built Environment, and resolved + evaluated through
// it by every database opened with that environment. Deliberately narrow
// (mirrors impl/rust/src/extension.rs and impl/go/extension.go):
//
//   - Functions only (no host types, no format bump).
//   - Ephemeral — reachable from ad-hoc queries while registered; NOT persisted (no DDL, no stored
//     index expression). The catalog-bound, versioned form is step 4.
//   - Strict — a NULL argument short-circuits to NULL before the kernel runs; the kernel never sees
//     a NULL.
//   - Scalar signatures — (name, ScalarType[]) → ScalarType, matched exactly first, then through
//     the implicit argument-coercion pass (spec/design/arg-coercion.md §4.2). A built-in overload
//     always wins over a host one (§4.2).
//   - Batched kernels (§4.2.1) — every host function runs through ONE column-in → column-out batch
//     kernel ABI. A host registers either a batch kernel (`batchKernel`) or a single-row kernel
//     (`kernel`), which a batch evaluation loops over. The executor prefetches a batch at the sites
//     that hold a chunk of rows and replays the row-at-a-time evaluation against it, so cost, error
//     order, and results are identical to batch-of-one by construction.
//
// `volatility` and `crossCore` are RECORDED forward-compat (only "immutable" will later admit
// constant-folding / index-backing; `crossCore` governs the §10 determinism ledger — jed has no
// runtime taint yet, for floats or anything). `cost` IS enforced: the declared static weight
// (cost.md §6 design (a)) is charged per call.

import { type EngineError, engineError } from "./errors.ts";
import { ALL_SCALAR_TYPES, type ScalarType } from "./types.ts";
import { Decimal } from "./decimal.ts";
import { decimalValue, float64Value, type Value } from "./value.ts";

// A host function's planning volatility (PostgreSQL's notion). Recorded on registration; only
// "immutable" will later admit constant-folding / an index-backing expression. This slice folds no
// host function regardless.
export type Volatility = "immutable" | "stable" | "volatile";

// A host scalar-function kernel: maps evaluated argument values to a result value, or throws an
// EngineError. STRICT — never invoked with a NULL argument (the engine short-circuits NULL→NULL).
export type HostKernel = (args: Value[]) => Value;

// A host BATCH kernel (extensibility.md §4.2.1) — the column-in → column-out ABI every host function
// is evaluated through. `args` is column-major: args[j][i] is argument j of row i; every column has
// the same length n ≥ 1 and holds no NULL (strict — a row with a NULL argument is never sent). The
// kernel pushes one result per row, in row order, to `out` (handed in empty). On a throw, out.length
// is the index of the failing row — the rows before it succeeded — so a batch raises for the same row
// a single-row call would. Must be the row-wise map of a scalar function: result i depends only on
// row i's arguments. A zero-argument kernel (no columns) is always handed exactly one row.
export type HostBatchKernel = (args: Value[][], out: Value[]) => void;

// One row's outcome of a batch call (HostFuncEntry.callBatch): a returned result, or the error the
// kernel raised for that row. A row after a reported error has no outcome (undefined — never
// computed).
export type HostOutcome = { value: Value } | { error: unknown };

// The spec a host passes to EnvironmentBuilder.registerFunction. `argTypes`/`result` are canonical
// ScalarType names ("i64", "text", …). Optional fields default to safe values (Volatile, not
// cross-core, unit cost) — matching Rust/Go so a function registered identically on every core
// accrues the same cost.
export interface HostFunctionSpec {
  name: string;
  argTypes: ScalarType[];
  result: ScalarType;
  // Exactly ONE of `kernel` (a single-row kernel) or `batchKernel` (a column-in → column-out batch
  // kernel, §4.2.1) must be given; either shape serves every evaluation.
  kernel?: HostKernel;
  batchKernel?: HostBatchKernel;
  volatility?: Volatility;
  crossCore?: boolean;
  cost?: bigint;
  // The COMPONENT IDENTITY (extensibility.md §7, delivery step 4) — a stable string the host chooses
  // (e.g. "com.example.geo/geo_hash") that names this function's *implementation* independently of its
  // SQL name. undefined (the default) is fine for an ad-hoc-query-only function; it is REQUIRED to use
  // the function in a PERSISTED index expression (§8.1), where it + semanticVersion form the resolved
  // dependency the file records and re-checks on reopen. Two registrations of the same SQL
  // name+signature but a different componentId are different implementations (a mismatch on reopen
  // makes a dependent index unusable, never a silent stale-key read).
  componentId?: string;
  // The SEMANTIC VERSION (extensibility.md §7) — bump it whenever a change to the function's results
  // would invalidate values/keys derived from it (a changed formula, a bug fix). A dependent index
  // persists the version it was built against; a mismatch on reopen forces a rebuild (the index is
  // unusable meanwhile), never a silent stale-key read. Default 0.
  semanticVersion?: number;
}

// A registered host function (the internal, defaults-resolved form).
export class HostFuncEntry {
  readonly name: string;
  readonly argTypes: ScalarType[];
  readonly result: ScalarType;
  readonly volatility: Volatility;
  readonly crossCore: boolean;
  readonly cost: bigint;
  // The host's stable component identity at registration, or null for an ad-hoc-query-only function
  // (extensibility.md §7, step 4). Required to back a persisted index (§8.1).
  readonly componentId: string | null;
  // The host's semantic version at registration (extensibility.md §7). Default 0.
  readonly semanticVersion: number;
  // The kernel the host registered — exactly one is set (§4.2.1): a single-row kernel is looped over
  // a batch, and a batch kernel is handed a one-row batch for a lone call.
  private readonly kernel: HostKernel | null;
  private readonly batchKernel: HostBatchKernel | null;

  constructor(
    name: string,
    spec: HostFunctionSpec,
    cost: bigint,
    kernel: HostKernel | null,
    batchKernel: HostBatchKernel | null,
  ) {
    this.name = name;
    this.argTypes = [...spec.argTypes];
    this.result = spec.result;
    this.volatility = spec.volatility ?? "volatile";
    this.crossCore = spec.crossCore ?? false;
    this.cost = cost;
    this.componentId = spec.componentId ?? null;
    this.semanticVersion = spec.semanticVersion ?? 0;
    this.kernel = kernel;
    this.batchKernel = batchKernel;
  }

  // Whether the executor may prefetch this function's results in a batch ahead of the row-at-a-time
  // replay (§4.2.1): a batch kernel (a single-row kernel is looped per row anyway, so prefetching it
  // would only add work and speculative calls) of any rung but "volatile", whose call set must stay
  // exactly the scalar one.
  batchable(): boolean {
    return this.batchKernel !== null && this.volatility !== "volatile";
  }

  // Call the kernel for ONE row (`args` non-NULL, one per parameter) — the batch-of-one path of every
  // site that does not prefetch. A single-row kernel's throw propagates unchanged; a batch kernel is
  // handed a one-row batch and held to the same shape rules as callBatch.
  callOne(args: Value[]): Value {
    args = args.map((v, i) => this.coerceArg(i, v));
    if (this.kernel !== null) return this.kernel(args);
    const outcome = this.callBatch(
      args.map((v) => [v]),
      1,
    )[0]!;
    if ("error" in outcome) throw outcome.error;
    return outcome.value;
  }

  // Run the kernel over `args` (column-major, n ≥ 1 rows, no NULLs) and return one outcome per row:
  // {value} for a returned result, {error} for the row the kernel reported failing, and undefined for
  // every row after it (never computed). Enforces the ABI shape (§4.2.1, all 22000): a normal return
  // with too few results fails at the first unanswered row; too many results, or a throw after
  // answering every row, fails at the first row. Results are NOT type-checked here — the replay does
  // that per row, so a type error surfaces at its own row.
  callBatch(args: Value[][], n: number): (HostOutcome | undefined)[] {
    if (this.needsCoercion(args)) args = args.map((col, i) => col.map((v) => this.coerceArg(i, v)));
    const out: Value[] = [];
    let failed = false;
    let err: unknown;
    try {
      if (this.batchKernel !== null) {
        this.batchKernel(args, out);
      } else {
        const k = this.kernel!;
        for (let i = 0; i < n; i++) out.push(k(args.map((col) => col[i]!)));
      }
    } catch (e) {
      failed = true;
      err = e;
    }
    const outcomes: (HostOutcome | undefined)[] = [];
    const got = out.length;
    if (got > n || (got === n && failed)) {
      outcomes.push({ error: this.shapeError(n, got) });
    } else {
      for (const v of out) outcomes.push({ value: v });
      if (got < n) outcomes.push({ error: failed ? err : this.shapeError(n, got) });
    }
    while (outcomes.length < n) outcomes.push(undefined);
    return outcomes;
  }

  // Convert an argument the coercion pass admitted to this function's declared type
  // (spec/design/arg-coercion.md §4): an integer for a decimal parameter at scale 0, an f32 for an
  // f64 one. Integer widths share one value form, so integer widening needs nothing. The kernel
  // only ever sees values of its declared types.
  private coerceArg(i: number, v: Value): Value {
    const t = this.argTypes[i];
    if (t === "decimal" && v.kind === "int") return decimalValue(Decimal.fromBigInt(v.int));
    if (t === "f64" && v.kind === "f32") return float64Value(v.value);
    return v;
  }

  // Whether any column of a batch holds a value coerceArg converts.
  private needsCoercion(args: Value[][]): boolean {
    return args.some((col, i) => {
      const t = this.argTypes[i];
      if (t === "decimal") return col.some((v) => v.kind === "int");
      if (t === "f64") return col.some((v) => v.kind === "f32");
      return false;
    });
  }

  private shapeError(n: number, got: number): EngineError {
    return engineError(
      "data_exception",
      `host function ${this.name} returned ${got} results for a batch of ${n} rows`,
    );
  }
}

// newHostFunction validates a host scalar-function registration against the functions already
// registered (`existing`) and returns its defaults-resolved entry — the check behind
// EnvironmentBuilder.registerFunction (spec/design/environment.md §5). Throws on a negative cost or not
// exactly one of kernel / batchKernel (22023), an unknown argument/result type (42704), or a second
// function with an identical (name, argTypes) signature (42723 — signature-level, not name-level: a
// host may overload a name across argument types, §4.2). A signature that shadows a built-in is
// accepted but never reached (built-ins win).
export function newHostFunction(
  existing: readonly HostFuncEntry[],
  spec: HostFunctionSpec,
): HostFuncEntry {
  const name = spec.name; // exact — host-API names are verbatim (grammar.md §3)
  const cost = spec.cost ?? 1n;
  if (cost < 0n)
    throw engineError(
      "invalid_parameter_value",
      `host function ${name}: cost must be non-negative`,
    );
  const kernel = spec.kernel ?? null;
  const batchKernel = spec.batchKernel ?? null;
  if ((kernel === null) === (batchKernel === null))
    throw engineError(
      "invalid_parameter_value",
      `host function ${name}: exactly one of kernel or batchKernel must be given`,
    );
  for (const t of spec.argTypes)
    if (!isScalarType(t))
      throw engineError("undefined_object", `host function ${name}: unknown argument type ${t}`);
  if (!isScalarType(spec.result))
    throw engineError(
      "undefined_object",
      `host function ${name}: unknown result type ${spec.result}`,
    );
  for (const g of existing)
    if (g.name === name && sameHostSig(g.argTypes, spec.argTypes))
      throw engineError(
        "duplicate_function",
        `host function ${name} already registered with this signature`,
      );
  return new HostFuncEntry(name, spec, cost, kernel, batchKernel);
}

// HostFunctions is the immutable set of host scalar functions an Environment carries
// (spec/design/environment.md §5, extensibility.md §7) — frozen when the environment is built and
// shared (by reference) by every database and session using it. Internal: a host registers through
// EnvironmentBuilder.registerFunction.
export class HostFunctions {
  private readonly functions: readonly HostFuncEntry[];

  constructor(functions: readonly HostFuncEntry[]) {
    this.functions = functions;
  }

  // The registered entries in registration order (an id is an index into it).
  entries(): readonly HostFuncEntry[] {
    return this.functions;
  }

  // Whether any registered host function has this (exact) name — the resolve-time routing gate.
  hasFunction(name: string): boolean {
    return this.functions.some((f) => f.name === name);
  }

  // Resolve (name, arg types) to a host-function id (a stable index) by exact scalar signature.
  // null ⇒ no host overload (the caller falls through to 42883).
  resolveHost(name: string, argTypes: ScalarType[]): number | null {
    const i = this.functions.findIndex((f) => f.name === name && sameHostSig(f.argTypes, argTypes));
    return i < 0 ? null : i;
  }

  // Every registered function with this name and arity, with its id, in registration order — the
  // host candidates of the argument-coercion pass (spec/design/arg-coercion.md §4.2).
  overloads(name: string, arity: number): { id: number; fn: HostFuncEntry }[] {
    const out: { id: number; fn: HostFuncEntry }[] = [];
    this.functions.forEach((fn, id) => {
      if (fn.name === name && fn.argTypes.length === arity) out.push({ id, fn });
    });
    return out;
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
