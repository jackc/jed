// Host extensions (spec/design/extensibility.md §4.2 / §5.1 / §7) — the injection seam a host
// application uses to register its own **scalar functions over existing types**. This is delivery
// step 3 (extensibility.md §14): runtime host functions, resolved + evaluated through a registry the
// host supplies at open/create and the engine freezes for the handle's lifetime. Deliberately narrow:
//
//   * Functions ONLY (no host types, no host containers) — they touch the function catalog, not the
//     type catalog, so no `format_version` bump and no persisted-catalog change.
//   * REGISTRY-DECLARED (Model B, extensibility.md §7/§14 step 4) — a host function is declared in
//     the registry and reachable from ad-hoc queries while registered; there is NO SQL DDL for it.
//     An `immutable` function that also carries a `component_id` + `semantic_version` may back a
//     **persisted index expression** (§8.1): the file records the resolved dependency (component id +
//     signature + version) and re-checks it on reopen — a missing / different-component / bumped-version
//     function makes the dependent index unusable (skipped for reads, refused for writes), never a
//     silent stale-key read. A function that is only ever used in ad-hoc queries needs neither
//     `component_id` nor a bumped version.
//   * STRICT — a NULL argument short-circuits to a NULL result before the kernel runs (§4.2); the
//     kernel never sees a NULL. Non-strict host functions are a follow-on.
//   * EXACT scalar signatures — `(name, [ScalarType]) → ScalarType`. Overload resolution matches the
//     resolved argument scalar types exactly; no implicit promotion (a promotion/family-pattern
//     signature is a follow-on). A built-in overload always wins over a host one (§4.2).
//   * BATCHED kernels (§4.2.1) — every host function runs through ONE column-in → column-out batch
//     kernel ABI. A host registers either a batch kernel ([`HostFunction::batched`]) or a single-row
//     kernel ([`HostFunction::new`]), which a batch evaluation loops over. The executor prefetches
//     a batch at the sites that hold a chunk of rows and replays the row-at-a-time evaluation against
//     it, so cost, error order, and results are identical to batch-of-one by construction.
//
// Two forward-looking declarations are RECORDED but not yet enforced this slice: `volatility` (only
// `Immutable` will later admit constant-folding / index-backing — no host function is folded now) and
// `cross_core` (governs the determinism ledger / taint containment of §10 — jed has no runtime taint
// mechanism yet, for floats or anything else, so this is carried for the future, not enforced). `cost`
// IS enforced: a host function's declared static weight is charged per call (cost.md §6 design (a)).

use crate::error::{EngineError, Result, SqlState};
use crate::types::ScalarType;
use crate::value::Value;

/// A host function's planning **volatility** (PostgreSQL's notion). Recorded on registration;
/// only `Immutable` will later admit constant-folding / an index-backing expression
/// (extensibility.md §4.2 / §8.1). This slice folds no host function regardless.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Volatility {
    /// Same inputs ⇒ same output, forever (no clock/entropy/state). The only rung that will later
    /// back an index expression or be constant-folded.
    Immutable,
    /// Stable within a single statement (may read committed state / settings), not across.
    Stable,
    /// May differ on every call (the safe default).
    Volatile,
}

/// A host scalar-function kernel: maps the evaluated argument values to a result value (or a raised
/// [`EngineError`]). **Strict** — never invoked with a NULL argument (the engine short-circuits
/// NULL→NULL, §4.2). `Send + Sync` so the registry rides the `Send + Sync` [`Database`](crate::Database)
/// handle across threads exactly like the rest of the shared core.
pub type HostKernel = Box<dyn Fn(&[Value]) -> Result<Value> + Send + Sync>;

/// A host **batch** kernel (extensibility.md §4.2.1) — the column-in → column-out ABI every host
/// function is evaluated through. `args` is column-major: `args[j][i]` is argument `j` of row `i`; every
/// column has the same length `n ≥ 1` and holds no NULL (strict — a row with a NULL argument is never
/// sent). The kernel appends one result per row, in row order, to `out` (handed in empty). On `Err`,
/// the number of results it appended is the index of the failing row — the rows before it succeeded —
/// so a batch raises for the same row a single-row call would. Must be the row-wise map of a scalar
/// function: result `i` depends only on row `i`'s arguments.
pub type HostBatchKernel = Box<dyn Fn(&[Vec<Value>], &mut Vec<Value>) -> Result<()> + Send + Sync>;

/// One registered host scalar function (extensibility.md §4.2). Built with [`HostFunction::new`]
/// (safe defaults: `Volatile`, not cross-core, unit cost) and refined with the builder setters.
pub struct HostFunction {
    /// Lowercased function name (SQL identifiers are case-insensitive).
    pub(crate) name: String,
    /// The exact scalar argument signature; overload resolution matches resolved arg types by
    /// equality against this (§4.2).
    pub(crate) arg_types: Vec<ScalarType>,
    /// The declared scalar result type. The engine checks the kernel's returned value against it.
    pub(crate) result: ScalarType,
    pub(crate) volatility: Volatility,
    pub(crate) cross_core: bool,
    /// The declared static cost weight (cost.md §6 design (a)), charged once per call. Non-negative.
    pub(crate) cost: i64,
    /// The **component identity** (extensibility.md §7, step 4) — a stable string the host chooses
    /// (e.g. `"com.example.geo/geo_hash"`) that identifies this function's *implementation*
    /// independently of its SQL name. `None` (the default) is fine for an ad-hoc-query-only function;
    /// it is **required** to use the function in a **persisted index expression** (§8.1), where it +
    /// [`semantic_version`](Self::semantic_version) form the resolved dependency the file records and
    /// re-checks on reopen. Two registrations of the same SQL name+signature but *different*
    /// `component_id` are different implementations (a mismatch on reopen makes a dependent index
    /// unusable, never a silent stale-key read).
    pub(crate) component_id: Option<String>,
    /// The **semantic version** (extensibility.md §7) — bump it whenever a change to the function's
    /// results would invalidate values/keys derived from it (a changed formula, a bug fix). A
    /// dependent index persists the version it was built against; a mismatch on reopen forces the
    /// index unusable (rebuild required), never a silent stale-key read. Default `0`.
    pub(crate) semantic_version: u32,
    pub(crate) kernel: Kernel,
}

/// The kernel a host registered — either shape serves every evaluation (§4.2.1): a single-row kernel
/// is looped over a batch, and a batch kernel is handed a one-row batch for a lone call.
pub(crate) enum Kernel {
    Row(HostKernel),
    Batch(HostBatchKernel),
}

impl HostFunction {
    /// A host scalar function with a **single-row** kernel and safe defaults — `Volatile`, not
    /// cross-core-deterministic, unit cost. Refine with [`volatility`](Self::volatility) /
    /// [`cross_core`](Self::cross_core) / [`cost`](Self::cost). A batch evaluation calls it once per
    /// row (§4.2.1); use [`batched`](Self::batched) to take a whole column per call instead.
    pub fn new(
        name: impl Into<String>,
        arg_types: Vec<ScalarType>,
        result: ScalarType,
        kernel: HostKernel,
    ) -> Self {
        HostFunction::with_kernel(name, arg_types, result, Kernel::Row(kernel))
    }

    /// A host scalar function with a **batch** kernel (extensibility.md §4.2.1) — one call per
    /// column of rows rather than per row, which amortizes a per-call boundary (an FFI upcall in a
    /// wrapped core). Same safe defaults as [`new`](Self::new).
    pub fn batched(
        name: impl Into<String>,
        arg_types: Vec<ScalarType>,
        result: ScalarType,
        kernel: HostBatchKernel,
    ) -> Self {
        HostFunction::with_kernel(name, arg_types, result, Kernel::Batch(kernel))
    }

    fn with_kernel(
        name: impl Into<String>,
        arg_types: Vec<ScalarType>,
        result: ScalarType,
        kernel: Kernel,
    ) -> Self {
        HostFunction {
            name: name.into().to_ascii_lowercase(),
            arg_types,
            result,
            volatility: Volatility::Volatile,
            cross_core: false,
            cost: 1,
            component_id: None,
            semantic_version: 0,
            kernel,
        }
    }

    /// Declare the planning volatility (default `Volatile`).
    pub fn volatility(mut self, v: Volatility) -> Self {
        self.volatility = v;
        self
    }

    /// Declare the function's results cross-core byte-identical (default `false`). Recorded for the
    /// determinism ledger (§10); not enforced this slice.
    pub fn cross_core(mut self, yes: bool) -> Self {
        self.cross_core = yes;
        self
    }

    /// Declare the per-call static cost weight (default `1`; cost.md §6 design (a)). Must be
    /// non-negative.
    pub fn cost(mut self, weight: i64) -> Self {
        self.cost = weight;
        self
    }

    /// Declare the **component identity** (extensibility.md §7, step 4) — a stable string that names
    /// this function's implementation independently of its SQL name. Required to use the function in a
    /// persisted index expression (§8.1). Default `None`.
    pub fn component_id(mut self, id: impl Into<String>) -> Self {
        self.component_id = Some(id.into());
        self
    }

    /// Declare the **semantic version** (extensibility.md §7) — bump it when a change to the results
    /// would invalidate keys/values derived from the function. A dependent index records this and a
    /// mismatch on reopen forces a rebuild, never a silent stale-key read. Default `0`.
    pub fn semantic_version(mut self, v: u32) -> Self {
        self.semantic_version = v;
        self
    }

    /// Whether the executor may prefetch this function's results in a batch ahead of the row-at-a-time
    /// replay (§4.2.1): any rung but `Volatile`, whose call set must stay exactly the scalar one.
    pub(crate) fn batchable(&self) -> bool {
        self.volatility != Volatility::Volatile
    }

    /// Call the kernel for ONE row (`args` non-NULL, one per parameter) — the batch-of-one path of
    /// every site that does not prefetch. A batch kernel is handed a one-row batch and held to the
    /// same shape rules as [`call_batch`](Self::call_batch).
    pub(crate) fn call_one(&self, args: Vec<Value>) -> Result<Value> {
        match &self.kernel {
            Kernel::Row(k) => k(&args),
            Kernel::Batch(_) => {
                let cols: Vec<Vec<Value>> = args.into_iter().map(|v| vec![v]).collect();
                self.call_batch(&cols, 1)
                    .pop()
                    .flatten()
                    .expect("a one-row batch yields one outcome")
            }
        }
    }

    /// Run the kernel over `args` (column-major, `n ≥ 1` rows, no NULLs) and return one outcome per
    /// row: `Some(Ok(v))` for a returned result, `Some(Err(e))` for the row the kernel reported
    /// failing, and `None` for every row after it (never computed). Enforces the ABI shape (§4.2.1,
    /// all `22000`): a successful return with too few results fails at the first unanswered row; too
    /// many results, or an error after answering every row, fails at the first row. Results are NOT
    /// type-checked here — the replay does that per row, so a type error surfaces at its own row.
    pub(crate) fn call_batch(&self, args: &[Vec<Value>], n: usize) -> Vec<Option<Result<Value>>> {
        let mut out = Vec::with_capacity(n);
        let res = match &self.kernel {
            Kernel::Batch(k) => k(args, &mut out),
            Kernel::Row(k) => (|| {
                let mut row = Vec::with_capacity(args.len());
                for i in 0..n {
                    row.clear();
                    row.extend(args.iter().map(|col| col[i].clone()));
                    out.push(k(&row)?);
                }
                Ok(())
            })(),
        };
        let mut outcomes: Vec<Option<Result<Value>>> = Vec::with_capacity(n);
        let got = out.len();
        if got > n || (got == n && res.is_err()) {
            outcomes.push(Some(Err(self.shape_error(n, got))));
        } else {
            outcomes.extend(out.into_iter().map(|v| Some(Ok(v))));
            if got < n {
                outcomes.push(Some(Err(match res {
                    Err(e) => e,
                    Ok(()) => self.shape_error(n, got),
                })));
            }
        }
        outcomes.resize_with(n, || None);
        outcomes
    }

    fn shape_error(&self, n: usize, got: usize) -> EngineError {
        EngineError::new(
            SqlState::DataException,
            format!(
                "host function {} returned {got} results for a batch of {n} rows",
                self.name
            ),
        )
    }

    /// Whether this function's signature matches `arg_tys` exactly (arity + per-position scalar type).
    fn signature_matches(&self, arg_tys: &[ScalarType]) -> bool {
        self.arg_types.len() == arg_tys.len()
            && std::iter::zip(&self.arg_types, arg_tys).all(|(a, b)| a == b)
    }
}

/// The immutable set of host extensions supplied at open/create and **frozen for the database
/// handle's lifetime** (extensibility.md §7). This slice holds host **scalar functions over existing
/// types** only. Cloned (`Arc`) into each session minted from the handle, so every session sees the
/// same functions; a streaming cursor's frozen engine shares it too.
#[derive(Default)]
pub struct ExtensionRegistry {
    functions: Vec<HostFunction>,
}

impl ExtensionRegistry {
    /// An empty registry — the default for a handle opened with no extensions.
    pub fn new() -> Self {
        ExtensionRegistry::default()
    }

    /// Register a host scalar function. Errors on a **negative cost** (`22023`) or a second function
    /// with an **identical `(name, arg_types)` signature** (`42723` — signature-level, not
    /// name-level: a host may overload a name across argument types, §4.2). A signature that shadows a
    /// built-in is accepted but never reached at resolve (built-ins win, §4.2).
    pub fn register_function(&mut self, f: HostFunction) -> Result<()> {
        if f.cost < 0 {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                format!("host function {}: cost must be non-negative", f.name),
            ));
        }
        if self
            .functions
            .iter()
            .any(|g| g.name == f.name && g.arg_types == f.arg_types)
        {
            return Err(EngineError::new(
                SqlState::DuplicateFunction,
                format!(
                    "host function {} already registered with this signature",
                    f.name
                ),
            ));
        }
        self.functions.push(f);
        Ok(())
    }

    /// Whether any registered host function has this (lowercased) name — the resolve-time routing
    /// gate (a host-only name is still routed to scalar-function resolution).
    pub(crate) fn has_function(&self, name: &str) -> bool {
        self.functions.iter().any(|f| f.name == name)
    }

    /// Resolve `(name, arg_types)` to a host-function **id** (a stable index into this frozen
    /// registry) by exact scalar signature. `None` ⇒ no host overload (the caller falls through to
    /// `42883`).
    pub(crate) fn resolve(&self, name: &str, arg_tys: &[ScalarType]) -> Option<usize> {
        self.functions
            .iter()
            .position(|f| f.name == name && f.signature_matches(arg_tys))
    }

    /// The registered function at `id` (an index returned by [`resolve`](Self::resolve); stable for
    /// the frozen registry's lifetime).
    pub(crate) fn function(&self, id: usize) -> &HostFunction {
        &self.functions[id]
    }
}

impl std::fmt::Debug for ExtensionRegistry {
    /// Prints the registered function names (their kernels are opaque `dyn Fn`, so the options
    /// structs that carry the registry can still `derive(Debug)`).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionRegistry")
            .field(
                "functions",
                &self.functions.iter().map(|g| &g.name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Whether `v` is a valid value for declared scalar result type `ty` — used to check a host
/// kernel's return against its declared `RETURNS` type, so a misbehaving host function cannot leak a
/// wrong-typed value into jed's strict type system (defense of jed's own invariants — the host owns
/// its consequences, CLAUDE.md §13, but jed's codecs/comparators must never see a type violation).
/// `NULL` is always valid (a strict function may still return NULL for non-null args).
pub(crate) fn value_matches_result(v: &Value, ty: ScalarType) -> bool {
    match v {
        Value::Null => true,
        Value::Int(_) => matches!(
            ty,
            ScalarType::Int16 | ScalarType::Int32 | ScalarType::Int64
        ),
        Value::Bool(_) => ty == ScalarType::Bool,
        Value::Float32(_) => ty == ScalarType::Float32,
        Value::Float64(_) => ty == ScalarType::Float64,
        Value::Text(_) => ty == ScalarType::Text,
        Value::Decimal(_) => ty == ScalarType::Decimal,
        Value::Bytea(_) => ty == ScalarType::Bytea,
        Value::Uuid(_) => ty == ScalarType::Uuid,
        Value::Timestamp(_) => ty == ScalarType::Timestamp,
        Value::Timestamptz(_) => ty == ScalarType::Timestamptz,
        Value::Date(_) => ty == ScalarType::Date,
        Value::Interval(_) => ty == ScalarType::Interval,
        Value::Json(_) => ty == ScalarType::Json,
        Value::Jsonb(_) => ty == ScalarType::Jsonb,
        Value::JsonPath(_) => ty == ScalarType::JsonPath,
        // Host scalar functions declare a scalar result; a container/unfetched value never matches.
        Value::Composite(_) | Value::Array(_) | Value::Range(_) | Value::Unfetched(_) => false,
    }
}
