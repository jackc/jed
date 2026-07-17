package jed

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
//   - Single-row kernels ("batch-of-one"); the vectorized ABI is a follow-on.
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

// HostFunction is a host scalar function to register (extensibility.md §4.2). Build it with
// NewHostFunction (safe defaults: Volatile, not cross-core, unit cost) and refine with the fluent
// setters. Argument/result types are canonical scalar type NAMES ("i64", "text", "f64", … — the
// spellings scalarTypeFromName accepts), resolved at RegisterFunction.
type HostFunction struct {
	name         string
	argTypeNames []string
	resultName   string
	volatility   Volatility
	crossCore    bool
	cost         int64
	kernel       HostKernel
}

// NewHostFunction builds a host scalar function with safe defaults — Volatile, not
// cross-core-deterministic, unit cost. Refine with WithVolatility / WithCrossCore / WithCost.
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

// WithVolatility declares the planning volatility (default Volatile).
func (f *HostFunction) WithVolatility(v Volatility) *HostFunction { f.volatility = v; return f }

// WithCrossCore declares the function's results cross-core byte-identical (default false). Recorded
// for the determinism ledger (§10); not enforced this slice.
func (f *HostFunction) WithCrossCore(b bool) *HostFunction { f.crossCore = b; return f }

// WithCost declares the per-call static cost weight (default 1; cost.md §6 design (a)). Must be
// non-negative.
func (f *HostFunction) WithCost(c int64) *HostFunction { f.cost = c; return f }

// hostFuncEntry is a registered host function with its types RESOLVED (the internal form the
// resolver + evaluator use).
type hostFuncEntry struct {
	name       string
	argTypes   []scalarType
	result     scalarType
	volatility Volatility
	crossCore  bool
	cost       int64
	kernel     HostKernel
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
		name:       f.name,
		argTypes:   argTypes,
		result:     result,
		volatility: f.volatility,
		crossCore:  f.crossCore,
		cost:       f.cost,
		kernel:     f.kernel,
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
