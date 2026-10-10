package jed

import "fmt"

// Host extensions (spec/design/extensibility.md §4.2 / §5.1 / §7) — the injection seam a host
// application uses to register its own SCALAR FUNCTIONS OVER EXISTING TYPES. This is delivery step 3
// (extensibility.md §14): runtime host functions, resolved + evaluated through a registry the host
// supplies at open/create and the engine freezes for the handle's lifetime. Deliberately narrow
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

// Volatility is a host function's planning volatility (PostgreSQL's notion). Recorded on
// registration; only VolatilityImmutable will later admit constant-folding / an index-backing
// expression (extensibility.md §4.2 / §8.1). This slice folds no host function regardless.
type Volatility int

const (
	// VolatilityImmutable: same inputs ⇒ same output, forever. The only rung that will later back an
	// index expression or be constant-folded.
	VolatilityImmutable Volatility = iota
	// VolatilityStable: stable within a single statement, not across.
	VolatilityStable
	// VolatilityVolatile: may differ on every call (the safe default).
	VolatilityVolatile
)

// HostKernel maps evaluated argument values to a result value (or an error). STRICT — never invoked
// with a NULL argument (the engine short-circuits NULL→NULL, §4.2).
type HostKernel func(args []Value) (Value, error)

// HostBatchKernel is a host BATCH kernel (extensibility.md §4.2.1) — the column-in → column-out ABI
// every host function is evaluated through. args is column-major: args[j][i] is argument j of row i;
// every column has the same length n ≥ 1 and holds no NULL (strict — a row with a NULL argument is
// never sent). The kernel appends one result per row, in row order, to out (handed in empty) and
// returns it. On error, the length of the returned slice is the index of the failing row — the rows
// before it succeeded — so a batch raises for the same row a single-row call would. Must be the
// row-wise map of a scalar function: result i depends only on row i's arguments.
type HostBatchKernel func(args [][]Value, out []Value) ([]Value, error)

// HostFunction is a host scalar function to register (extensibility.md §4.2). Build it with
// NewHostFunction (a single-row kernel) or NewHostBatchFunction (a batch kernel, §4.2.1) — safe
// defaults: Volatile, not cross-core, unit cost — and refine with the fluent setters. Argument/result types are canonical scalar type NAMES ("i64", "text", "f64", … — the
// spellings scalarTypeFromName accepts), resolved at RegisterFunction.
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

// NewHostFunction builds a host scalar function with a SINGLE-ROW kernel and safe defaults — Volatile,
// not cross-core-deterministic, unit cost. Refine with WithVolatility / WithCrossCore / WithCost. A
// batch evaluation calls it once per row (§4.2.1); use NewHostBatchFunction to take a whole column per
// call instead.
func NewHostFunction(name string, argTypes []string, resultType string, kernel HostKernel) *HostFunction {
	return &HostFunction{
		name:         toLowerASCII(name),
		argTypeNames: argTypes,
		resultName:   resultType,
		volatility:   VolatilityVolatile,
		crossCore:    false,
		cost:         1,
		kernel:       kernel,
	}
}

// NewHostBatchFunction builds a host scalar function with a BATCH kernel (extensibility.md §4.2.1) —
// one call per column of rows rather than per row, which amortizes a per-call boundary. Same safe
// defaults as NewHostFunction.
func NewHostBatchFunction(name string, argTypes []string, resultType string, kernel HostBatchKernel) *HostFunction {
	return &HostFunction{
		name:         toLowerASCII(name),
		argTypeNames: argTypes,
		resultName:   resultType,
		volatility:   VolatilityVolatile,
		crossCore:    false,
		cost:         1,
		batchKernel:  kernel,
	}
}

// WithVolatility declares the planning volatility (default Volatile).
func (f *HostFunction) WithVolatility(v Volatility) *HostFunction { f.volatility = v; return f }

// WithCrossCore declares the function's results cross-core byte-identical (default false). Recorded
// for the determinism ledger (§10); not enforced this slice.
func (f *HostFunction) WithCrossCore(b bool) *HostFunction { f.crossCore = b; return f }

// WithCost declares the per-call static cost weight (default 1; cost.md §6 design (a)). Must be
// non-negative.
func (f *HostFunction) WithCost(c int64) *HostFunction { f.cost = c; return f }

// WithComponentID declares the COMPONENT IDENTITY (extensibility.md §7, step 4) — a stable string that
// names this function's implementation independently of its SQL name. Required to use the function in a
// persisted index expression (§8.1). Default unset (nil).
func (f *HostFunction) WithComponentID(id string) *HostFunction { f.componentID = &id; return f }

// WithSemanticVersion declares the SEMANTIC VERSION (extensibility.md §7) — bump it when a change to
// the results would invalidate keys/values derived from the function. A dependent index records this
// and a mismatch on reopen forces a rebuild, never a silent stale-key read. Default 0.
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

// hostOutcome is one row's outcome of a batch kernel call (§4.2.1): the returned value, the error the
// kernel reported for this row, or — computed false — a row after the failing one, never computed.
type hostOutcome struct {
	value    Value
	err      error
	computed bool
}

// batchable reports whether the executor may prefetch this function's results in a batch ahead of the
// row-at-a-time replay (§4.2.1): any rung but Volatile, whose call set must stay exactly the scalar one.
func (f *hostFuncEntry) batchable() bool { return f.volatility != VolatilityVolatile }

// callOne calls the kernel for ONE row (args non-NULL, one per parameter) — the batch-of-one path of
// every site that does not prefetch. A batch kernel is handed a one-row batch and held to the same
// shape rules as callBatch.
func (f *hostFuncEntry) callOne(args []Value) (Value, error) {
	if f.batchKernel == nil {
		return f.kernel(args)
	}
	cols := make([][]Value, len(args))
	for j, v := range args {
		cols[j] = []Value{v}
	}
	o := f.callBatch(cols, 1)[0]
	return o.value, o.err
}

// callBatch runs the kernel over args (column-major, n ≥ 1 rows, no NULLs) and returns one outcome per
// row: a value for a returned result, an error for the row the kernel reported failing, and a
// not-computed outcome for every row after it. Enforces the ABI shape (§4.2.1, all 22000): a successful
// return with too few results fails at the first unanswered row; too many results, or an error after
// answering every row, fails at the first row. Results are NOT type-checked here — the replay does
// that per row, so a type error surfaces at its own row.
func (f *hostFuncEntry) callBatch(args [][]Value, n int) []hostOutcome {
	var out []Value
	var err error
	if f.batchKernel != nil {
		out, err = f.batchKernel(args, make([]Value, 0, n))
	} else {
		out = make([]Value, 0, n)
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
	outcomes := make([]hostOutcome, n)
	got := len(out)
	if got > n || (got == n && err != nil) {
		outcomes[0] = hostOutcome{err: f.shapeError(n, got), computed: true}
		return outcomes
	}
	for i, v := range out {
		outcomes[i] = hostOutcome{value: v, computed: true}
	}
	if got < n {
		if err == nil {
			err = f.shapeError(n, got)
		}
		outcomes[got] = hostOutcome{err: err, computed: true}
	}
	return outcomes
}

func (f *hostFuncEntry) shapeError(n, got int) error {
	return newError(DataException, fmt.Sprintf("host function %s returned %d results for a batch of %d rows", f.name, got, n))
}

// ExtensionRegistry is the immutable set of host extensions supplied at open/create and FROZEN for
// the database handle's lifetime (extensibility.md §7). This slice holds host scalar functions over
// existing types only. Shared (by pointer) across every session minted from the handle; a streaming
// cursor's frozen engine copies the sessionState struct and so shares it too.
type ExtensionRegistry struct {
	functions []hostFuncEntry
}

// NewExtensionRegistry returns an empty registry.
func NewExtensionRegistry() *ExtensionRegistry { return &ExtensionRegistry{} }

// RegisterFunction registers a host scalar function. Errors on a negative cost (22023), an unknown
// argument/result type name (42704), or a second function with an identical (name, arg-types)
// signature (42723 — signature-level, not name-level: a host may overload a name across argument
// types, §4.2). A signature that shadows a built-in is accepted but never reached (built-ins win).
func (r *ExtensionRegistry) RegisterFunction(f *HostFunction) error {
	if f.cost < 0 {
		return newError(InvalidParameterValue, "host function "+f.name+": cost must be non-negative")
	}
	argTypes := make([]scalarType, len(f.argTypeNames))
	for i, n := range f.argTypeNames {
		st, ok := scalarTypeFromName(n)
		if !ok {
			return newError(UndefinedObject, "host function "+f.name+": unknown argument type "+n)
		}
		argTypes[i] = st
	}
	result, ok := scalarTypeFromName(f.resultName)
	if !ok {
		return newError(UndefinedObject, "host function "+f.name+": unknown result type "+f.resultName)
	}
	for i := range r.functions {
		if r.functions[i].name == f.name && sameHostSig(r.functions[i].argTypes, argTypes) {
			return newError(DuplicateFunction, "host function "+f.name+" already registered with this signature")
		}
	}
	r.functions = append(r.functions, hostFuncEntry{
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
	})
	return nil
}

// hasFunction reports whether any registered host function has this (lowercased) name — the
// resolve-time routing gate. nil-safe (a handle with no extensions).
func (r *ExtensionRegistry) hasFunction(name string) bool {
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
func (r *ExtensionRegistry) resolveHost(name string, argTypes []scalarType) (int, bool) {
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

// function returns the registered function at id (an index from resolveHost).
func (r *ExtensionRegistry) function(id int) *hostFuncEntry { return &r.functions[id] }

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
	switch v.Kind {
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
	case ValUuid:
		return ty == scalarUuid
	case ValTimestamp:
		return ty == scalarTimestamp
	case ValTimestamptz:
		return ty == scalarTimestamptz
	case ValDate:
		return ty == scalarDate
	case ValInterval:
		return ty == scalarInterval
	case ValJson:
		return ty == scalarJson
	case ValJsonb:
		return ty == scalarJsonb
	case ValJsonPath:
		return ty == scalarJsonPath
	default:
		// ValComposite / ValArray / ValRange / ValUnfetched: a host scalar function declares a
		// scalar result; a container/unfetched value never matches.
		return false
	}
}
