package jed

import "fmt"

// Host extensions (spec/design/extensibility.md §4.2 / §5.1 / §7) — the injection seam a host
// application uses to register its own SCALAR FUNCTIONS OVER EXISTING TYPES. This is delivery step 3
// (extensibility.md §14): runtime host functions, registered on an EnvironmentBuilder
// (environment.md §5) and resolved + evaluated through the frozen registry of the handle's
// Environment. Deliberately narrow
// (mirrors impl/rust/src/extension.rs):
//
//   - Functions only (no host types, no format bump).
//   - Ephemeral — reachable from ad-hoc queries while registered; NOT persisted (no DDL, no stored
//     index expression). The catalog-bound, versioned form is step 4.
//   - Strict — a NULL argument short-circuits to NULL before the kernel runs; the kernel never sees
//     a NULL.
//   - Exact scalar signatures — (name, []scalarType) → scalarType, matched by equality (no implicit
//     promotion). A built-in overload always wins over a host one (§4.2).
//   - Batched kernels (§4.2.1) — every host function runs through ONE column-in → column-out batch
//     kernel ABI. A host registers either a batch kernel (NewHostBatchFunction) or a single-row kernel
//     (NewHostFunction), which a batch evaluation loops over. The executor prefetches a batch at the
//     sites that hold a chunk of rows and replays the row-at-a-time evaluation against it, so cost,
//     error order, and results are identical to batch-of-one by construction.
//
// `Volatility` and `CrossCore` are RECORDED forward-compat (only Immutable will later admit
// constant-folding / index-backing; CrossCore governs the §10 determinism ledger — jed has no
// runtime taint yet, for floats or anything). `Cost` IS enforced: the declared static weight
// (cost.md §6 design (a)) is charged per call.

// Volatility declares how stable a host function's results are, as PostgreSQL's function
// volatility does. Set it with [HostFunction.WithVolatility]; the default is VolatilityVolatile.
type Volatility int

// The volatility levels, from most to least stable.
const (
	// VolatilityImmutable declares that the function always returns the same result for the same
	// arguments. Only an immutable function may appear in an index expression.
	VolatilityImmutable Volatility = iota
	// VolatilityStable declares that the function returns the same result for the same arguments
	// within one statement, but not necessarily across statements.
	VolatilityStable
	// VolatilityVolatile declares that the function may return a different result on every call.
	// It is the default. A volatile function is always called exactly once per row it is
	// evaluated for; a more stable batch function may be called ahead on a batch of rows.
	VolatilityVolatile
)

// HostKernel computes a host function's result for one row. args holds one value per declared
// argument, already converted to the declared types. Host functions are strict: when any argument
// is NULL the result is NULL and the kernel is not called, so args never contains a NULL. A
// returned error fails the statement.
type HostKernel func(args []Value) (Value, error)

// HostBatchKernel computes a host function's results for a batch of rows in one call. args is
// column-major: args[j][i] is argument j of row i. Every column has the same length n, at least 1,
// and holds no NULL: as with [HostKernel], a row with a NULL argument gets a NULL result without
// being sent. A function with no arguments is always given exactly one row.
//
// The kernel appends one result per row, in row order, to out (passed in empty) and returns it.
// To fail, it returns the results of the rows before the failing one together with the error, so
// the number of results returned is the index of the failing row and the statement fails at the
// same row a one-row-at-a-time call would. Returning too few or too many results otherwise fails
// with SQLSTATE 22000. Result i must depend only on row i's arguments.
type HostBatchKernel func(args [][]Value, out []Value) ([]Value, error)

// HostFunction describes a scalar function written in Go that SQL can call. Create one with
// [NewHostFunction] or [NewHostBatchFunction], adjust it with the With methods, and register it
// with [EnvironmentBuilder.RegisterFunction]. Host functions take and return existing SQL types;
// they live in the [Environment], not in the database file, so every process that opens a
// database must register the functions its queries, indexes and CHECK constraints use.
//
// Argument and result types are lowercase SQL type names: "int2"/"smallint", "int4"/"int"/
// "integer", "int8"/"bigint", "numeric"/"decimal", "real"/"float4", "double precision"/"float8"/
// "float", "text"/"varchar", "boolean"/"bool", "bytea", "uuid", "date", "timestamp",
// "timestamptz", "interval", "json", "jsonb" and "jsonpath". Unknown names fail at registration.
//
// When a host function has the same name and argument types as a built-in function, the
// built-in is called.
type HostFunction struct {
	name         string
	argTypeNames []string
	resultName   string
	volatility   Volatility
	crossCore    bool
	cost         int64
	// componentID is the host's stable COMPONENT IDENTITY (extensibility.md §7, step 4) — a string
	// naming this function's IMPLEMENTATION independently of its SQL name (e.g. "com.example/geo_hash").
	// nil (the default) is fine for an ad-hoc-query-only function; it is REQUIRED to use the function in
	// a PERSISTED index expression (§8.1), where it + semanticVersion form the resolved dependency the
	// file records and re-checks on reopen (a mismatch makes the dependent index unusable, never a
	// silent stale-key read).
	componentID *string
	// semanticVersion is the host's SEMANTIC VERSION (extensibility.md §7) — bump it whenever a change
	// to the function's results would invalidate values/keys derived from it. A dependent index persists
	// the version it was built against; a mismatch on reopen forces a rebuild, never a silent stale-key
	// read. Default 0.
	semanticVersion uint32
	// Exactly one of kernel / batchKernel is set — either shape serves every evaluation (§4.2.1): a
	// single-row kernel is looped over a batch, and a batch kernel is handed a one-row batch for a lone
	// call.
	kernel      HostKernel
	batchKernel HostBatchKernel
}

// NewHostFunction returns a host function named name that takes arguments of argTypes, returns
// resultType, and computes each row with kernel. It starts volatile, not cross-core, with cost 1
// and no component identity. Use [NewHostBatchFunction] to process many rows per call instead.
func NewHostFunction(name string, argTypes []string, resultType string, kernel HostKernel) *HostFunction {
	return &HostFunction{
		name:         name,
		argTypeNames: argTypes,
		resultName:   resultType,
		volatility:   VolatilityVolatile,
		crossCore:    false,
		cost:         1,
		kernel:       kernel,
	}
}

// NewHostBatchFunction is like [NewHostFunction] but takes a batch kernel, called once for many
// rows, which amortizes per-call overhead. It has the same defaults.
func NewHostBatchFunction(name string, argTypes []string, resultType string, kernel HostBatchKernel) *HostFunction {
	return &HostFunction{
		name:         name,
		argTypeNames: argTypes,
		resultName:   resultType,
		volatility:   VolatilityVolatile,
		crossCore:    false,
		cost:         1,
		batchKernel:  kernel,
	}
}

// WithVolatility sets the function's volatility (default VolatilityVolatile) and returns f.
func (f *HostFunction) WithVolatility(v Volatility) *HostFunction { f.volatility = v; return f }

// WithCrossCore declares whether the function returns byte-identical results in every jed
// implementation (default false) and returns f. It is currently recorded only.
func (f *HostFunction) WithCrossCore(b bool) *HostFunction { f.crossCore = b; return f }

// WithCost sets the cost charged to the statement's execution-cost meter for each call (default
// 1) and returns f. A negative cost fails at registration with SQLSTATE 22023.
func (f *HostFunction) WithCost(c int64) *HostFunction { f.cost = c; return f }

// WithComponentID sets the function's component identity and returns f. The identity is a stable
// string naming the implementation independently of its SQL name, such as
// "com.example/geo_hash". It is required to use the function in an index expression or a CHECK
// constraint (otherwise creating the index or constraint fails with SQLSTATE 42P17); the database
// records it and checks it when the index or constraint is used. Default unset.
func (f *HostFunction) WithComponentID(id string) *HostFunction { f.componentID = &id; return f }

// WithSemanticVersion sets the function's semantic version (default 0) and returns f. Increase it
// whenever a change to the function's results would invalidate stored data derived from it. An
// index or CHECK constraint records the component identity and version it was built with; when
// the registered function no longer matches, queries stop using the index, writes to the table
// and statements that evaluate the constraint fail with SQLSTATE XX002, and the index must be
// rebuilt or the constraint re-added.
func (f *HostFunction) WithSemanticVersion(v uint32) *HostFunction { f.semanticVersion = v; return f }

// hostFuncEntry is a registered host function with its types RESOLVED (the internal form the
// resolver + evaluator use).
type hostFuncEntry struct {
	name       string
	argTypes   []scalarType
	result     scalarType
	volatility Volatility
	crossCore  bool
	cost       int64
	// componentID / semanticVersion carry the step-4 identity (extensibility.md §7/§8.1): the stable
	// implementation id + version an index-backing function pins into the file's dependency list. nil
	// componentID ⇒ the function may NOT back a persisted index expression (42P17 at CREATE INDEX).
	componentID     *string
	semanticVersion uint32
	kernel          HostKernel      // the single-row kernel, or nil when batchKernel is set
	batchKernel     HostBatchKernel // the batch kernel, or nil when kernel is set
}

// batchable reports whether the executor may prefetch this function's results in a batch ahead of the
// row-at-a-time replay (§4.2.1): a batch kernel (a single-row kernel is looped per row anyway, so
// prefetching it would only add work and speculative calls) of any rung but Volatile, whose call set
// must stay exactly the scalar one.
func (f *hostFuncEntry) batchable() bool {
	return f.batchKernel != nil && f.volatility != VolatilityVolatile
}

// callOne calls the kernel for ONE row (args non-NULL, one per parameter) — the batch-of-one path of
// every site that does not prefetch. A batch kernel is handed a one-row batch and held to the same
// shape rules as callBatch.
func (f *hostFuncEntry) callOne(args []Value) (Value, error) {
	args = f.coerceRow(args)
	if f.batchKernel == nil {
		return f.kernel(args)
	}
	cols := make([][]Value, len(args))
	for j, v := range args {
		cols[j] = []Value{v}
	}
	results, err := f.callBatch(cols, 1)
	if err != nil {
		return Value{}, err
	}
	return results[0], nil
}

// callBatch runs the kernel over args (column-major, n ≥ 1 rows, no NULLs) and returns the results of
// the leading rows the kernel answered and, when that is fewer than n, the error of the row after them
// (every later row was never computed); so err == nil ⇒ len(results) == n. Enforces the ABI shape
// (§4.2.1, all 22000): a successful return with too few results fails at the first unanswered row; too
// many results, or an error after answering every row, fails at the first row. Results are NOT
// type-checked here — the replay does that per row, so a type error surfaces at its own row.
func (f *hostFuncEntry) callBatch(args [][]Value, n int) ([]Value, error) {
	args = f.coerceBatch(args)
	out := make([]Value, 0, n)
	var err error
	if f.batchKernel != nil {
		out, err = f.batchKernel(args, out)
	} else {
		row := make([]Value, len(args))
		for i := 0; i < n; i++ {
			for j, col := range args {
				row[j] = col[i]
			}
			var v Value
			if v, err = f.kernel(row); err != nil {
				break
			}
			out = append(out, v)
		}
	}
	got := len(out)
	if got > n || (got == n && err != nil) {
		return out[:0], f.shapeError(n, got)
	}
	if got < n && err == nil {
		err = f.shapeError(n, got)
	}
	return out, err
}

// coerceArg converts an argument the coercion pass admitted to this function's declared type at
// position i (spec/design/arg-coercion.md §4): an integer for a decimal parameter at scale 0, an f32
// for an f64 one. Integer widths share one value form, so integer widening needs nothing. The kernel
// only ever sees values of its declared types. needed is false when v is already of that type.
func (f *hostFuncEntry) coerceArg(i int, v Value) (out Value, needed bool) {
	switch {
	case f.argTypes[i] == scalarDecimal && v.kind == ValInt:
		return DecimalValue(decimalFromInt64(v.num)), true
	case f.argTypes[i] == scalarFloat64 && v.kind == ValFloat32:
		return Float64Value(float64(v.f32())), true
	}
	return v, false
}

// coerceRow applies coerceArg to one row's arguments, copying the row only when a value converts
// (the caller's slice is never mutated).
func (f *hostFuncEntry) coerceRow(args []Value) []Value {
	var out []Value
	for i, v := range args {
		c, needed := f.coerceArg(i, v)
		if !needed {
			continue
		}
		if out == nil {
			out = append([]Value(nil), args...)
		}
		out[i] = c
	}
	if out == nil {
		return args
	}
	return out
}

// coerceBatch applies coerceArg to a column-major batch, copying only the columns that hold a value
// that converts.
func (f *hostFuncEntry) coerceBatch(args [][]Value) [][]Value {
	var out [][]Value
	for i, col := range args {
		coerced := f.coerceColumn(i, col)
		if coerced == nil {
			continue
		}
		if out == nil {
			out = append([][]Value(nil), args...)
		}
		out[i] = coerced
	}
	if out == nil {
		return args
	}
	return out
}

// coerceColumn converts column i of a batch, or returns nil when no value in it converts.
func (f *hostFuncEntry) coerceColumn(i int, col []Value) []Value {
	var out []Value
	for j, v := range col {
		c, needed := f.coerceArg(i, v)
		if !needed {
			continue
		}
		if out == nil {
			out = append([]Value(nil), col...)
		}
		out[j] = c
	}
	return out
}

func (f *hostFuncEntry) shapeError(n, got int) error {
	return newError(DataException, fmt.Sprintf("host function %s returned %d results for a batch of %d rows", f.name, got, n))
}

// hostRegistry is the frozen set of host functions an Environment holds (extensibility.md §7,
// environment.md §5) — built once by EnvironmentBuilder.Build and never mutated, so it is shared (by
// pointer) across every session of every handle using that environment; a function's index is its
// stable resolved id.
type hostRegistry struct {
	functions []hostFuncEntry
}

// resolveHostFunction validates a host function and resolves its type names (the registration half
// of EnvironmentBuilder.RegisterFunction): a negative cost is 22023, an unknown argument/result type
// name 42704.
func resolveHostFunction(f *HostFunction) (hostFuncEntry, error) {
	if f.cost < 0 {
		return hostFuncEntry{}, newError(InvalidParameterValue, "host function "+f.name+": cost must be non-negative")
	}
	argTypes := make([]scalarType, len(f.argTypeNames))
	for i, n := range f.argTypeNames {
		st, ok := scalarTypeFromName(n)
		if !ok {
			return hostFuncEntry{}, newError(UndefinedObject, "host function "+f.name+": unknown argument type "+n)
		}
		argTypes[i] = st
	}
	result, ok := scalarTypeFromName(f.resultName)
	if !ok {
		return hostFuncEntry{}, newError(UndefinedObject, "host function "+f.name+": unknown result type "+f.resultName)
	}
	return hostFuncEntry{
		name:            f.name,
		argTypes:        argTypes,
		result:          result,
		volatility:      f.volatility,
		crossCore:       f.crossCore,
		cost:            f.cost,
		componentID:     f.componentID,
		semanticVersion: f.semanticVersion,
		kernel:          f.kernel,
		batchKernel:     f.batchKernel,
	}, nil
}

// hasFunction reports whether any registered host function has this (exact) name — the
// resolve-time routing gate. nil-safe.
func (r *hostRegistry) hasFunction(name string) bool {
	if r == nil {
		return false
	}
	for i := range r.functions {
		if r.functions[i].name == name {
			return true
		}
	}
	return false
}

// resolveHost resolves (name, arg types) to a host-function id (a stable index) by exact scalar
// signature. Reports false ⇒ no host overload (the caller falls through to 42883). nil-safe.
func (r *hostRegistry) resolveHost(name string, argTypes []scalarType) (int, bool) {
	if r == nil {
		return 0, false
	}
	for i := range r.functions {
		if r.functions[i].name == name && sameHostSig(r.functions[i].argTypes, argTypes) {
			return i, true
		}
	}
	return 0, false
}

// overloads lists the ids of every registered function with this name and arity, in registration
// order — the host candidates of the argument-coercion pass (spec/design/arg-coercion.md §4.2).
// nil-safe.
func (r *hostRegistry) overloads(name string, arity int) []int {
	if r == nil {
		return nil
	}
	var ids []int
	for i := range r.functions {
		if r.functions[i].name == name && len(r.functions[i].argTypes) == arity {
			ids = append(ids, i)
		}
	}
	return ids
}

// function returns the registered function at id (an index from resolveHost).
func (r *hostRegistry) function(id int) *hostFuncEntry { return &r.functions[id] }

func sameHostSig(a, b []scalarType) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

// valueMatchesResult reports whether v is a valid value for declared scalar result type ty — used
// to check a host kernel's return against its declared result type, so a misbehaving host function
// cannot leak a wrong-typed value into jed's strict type system (defense of jed's own invariants —
// the host owns its consequences, CLAUDE.md §13, but jed's codecs/comparators must never see a type
// violation). NULL is always valid (a strict function may still return NULL for non-null args).
func valueMatchesResult(v Value, ty scalarType) bool {
	switch v.kind {
	case ValNull:
		return true
	case ValInt:
		return ty == scalarInt16 || ty == scalarInt32 || ty == scalarInt64
	case ValBool:
		return ty == scalarBool
	case ValFloat32:
		return ty == scalarFloat32
	case ValFloat64:
		return ty == scalarFloat64
	case ValText:
		return ty == scalarText
	case ValDecimal:
		return ty == scalarDecimal
	case ValBytea:
		return ty == scalarBytea
	case ValUUID:
		return ty == scalarUuid
	case ValTimestamp:
		return ty == scalarTimestamp
	case ValTimestamptz:
		return ty == scalarTimestamptz
	case ValDate:
		return ty == scalarDate
	case ValInterval:
		return ty == scalarInterval
	case ValJSON:
		return ty == scalarJson
	case ValJSONB:
		return ty == scalarJsonb
	case ValJSONPath:
		return ty == scalarJsonPath
	default:
		// ValComposite / ValArray / ValRange / ValUnfetched: a host scalar function declares a
		// scalar result; a container/unfetched value never matches.
		return false
	}
}
