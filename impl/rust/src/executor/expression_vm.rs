//! Opt-in, execution-owned expression VM experiment. See expression-vm-experiment.md.
//! The immutable program is independent of its compact, reusable scalar registers. A cursor
//! compiles each eligible expression once; no expression address, global cache, or plan mutation.
use super::*;
use std::cell::Cell;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static COUNTERS: Cell<Stats> = const { Cell::new(Stats { compiled: 0, fallbacks: 0, program_bytes: 0, scratch_bytes: 0, evaluations: 0 }) };
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Stats {
    pub compiled: u64,
    pub fallbacks: u64,
    /// Sum of retained instruction-vector capacities for successful compilations since reset;
    /// divide by query count for these repeated single-query workloads. Excludes source trees,
    /// fixed owner structs, failed compiler scratch, allocator metadata and total allocations.
    pub program_bytes: u64,
    /// Corresponding sum of execution register-vector capacities (the registers contain no heap
    /// values). This is diagnostic capacity, not the deterministic SQL query-memory account.
    pub scratch_bytes: u64,
    pub evaluations: u64,
}

pub fn set_enabled(enabled: bool) {
    ENABLED.with(|e| e.set(enabled));
}
pub fn reset_stats() {
    COUNTERS.with(|s| s.set(Stats::default()));
}
pub fn stats() -> Stats {
    COUNTERS.with(Cell::get)
}
fn enabled() -> bool {
    ENABLED.with(Cell::get)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int,
    Bool,
    Null,
    Other,
}
#[derive(Clone, Copy)]
enum Slot {
    Int(i64),
    Bool(bool),
    Null,
}
impl Slot {
    fn read(v: &Value) -> Self {
        match v {
            Value::Int(n) => Self::Int(*n),
            Value::Bool(b) => Self::Bool(*b),
            Value::Null => Self::Null,
            _ => unreachable!("VM compiler admits only statically fixed integer/boolean slots"),
        }
    }
    fn value(self) -> Value {
        match self {
            Self::Int(n) => Value::Int(n),
            Self::Bool(b) => Value::Bool(b),
            Self::Null => Value::Null,
        }
    }
    fn true_(self) -> bool {
        matches!(self, Self::Bool(true))
    }
}
#[derive(Clone, Copy)]
enum Test {
    False,
    True,
    NotTrue,
    NotNull,
}
#[derive(Clone, Copy)]
enum Op {
    Enter(i64),
    Constant(usize, Slot),
    Column(usize, usize),
    Param(usize, usize),
    Arithmetic(usize, ArithOp, ScalarType),
    Negate(usize, ScalarType),
    Cast(usize, ScalarType),
    Compare(usize, CmpOp),
    Not(usize),
    IsNull(usize, bool),
    And(usize),
    Or(usize),
    Branch(usize, Test, usize),
    Jump(usize),
}
struct Program {
    ops: Vec<Op>,
    slots: usize,
}
pub(crate) struct Runner {
    program: Program,
    slots: Vec<Slot>,
}
struct Compiler<'a> {
    program: Program,
    columns: &'a [Kind],
    params: &'a [Kind],
}
impl Compiler<'_> {
    fn emit(&mut self, e: &RExpr, dst: usize, filter: bool) -> Option<Kind> {
        self.program.slots = self.program.slots.max(dst + 1);
        let cost = match e {
            RExpr::Column(_)
            | RExpr::Param(_)
            | RExpr::ConstInt(_)
            | RExpr::ConstBool(_)
            | RExpr::ConstNull => 0,
            RExpr::Arith { op, .. } => operator_cost(op.op_name()),
            RExpr::Compare { op, .. } => operator_cost(op.op_name()),
            RExpr::Neg { .. } => operator_cost("neg"),
            RExpr::Not(_) => operator_cost("not"),
            RExpr::And(..) => operator_cost("and"),
            RExpr::Or(..) => operator_cost("or"),
            _ => COSTS.operator_eval,
        };
        self.program.ops.push(Op::Enter(cost));
        let kind = match e {
            RExpr::ConstInt(n) => {
                self.program.ops.push(Op::Constant(dst, Slot::Int(*n)));
                Kind::Int
            }
            RExpr::ConstBool(b) => {
                self.program.ops.push(Op::Constant(dst, Slot::Bool(*b)));
                Kind::Bool
            }
            RExpr::ConstNull => {
                self.program.ops.push(Op::Constant(dst, Slot::Null));
                Kind::Null
            }
            RExpr::Column(i) => {
                self.program.ops.push(Op::Column(dst, *i));
                *self.columns.get(*i)?
            }
            RExpr::Param(i) => {
                self.program.ops.push(Op::Param(dst, *i));
                *self.params.get(*i)?
            }
            RExpr::Arith {
                op,
                lhs,
                rhs,
                result,
            } if result.is_integer() => {
                if !integer(self.emit(lhs, dst, false)?)
                    || !integer(self.emit(rhs, dst + 1, false)?)
                {
                    return None;
                }
                self.program.ops.push(Op::Arithmetic(dst, *op, *result));
                Kind::Int
            }
            RExpr::Neg { operand, result } if result.is_integer() => {
                if !integer(self.emit(operand, dst, false)?) {
                    return None;
                }
                self.program.ops.push(Op::Negate(dst, *result));
                Kind::Int
            }
            RExpr::Cast { inner, target, .. } if target.is_integer() || target.is_bool() => {
                self.emit(inner, dst, false)?;
                self.program.ops.push(Op::Cast(dst, *target));
                if target.is_bool() {
                    Kind::Bool
                } else {
                    Kind::Int
                }
            }
            RExpr::Compare {
                op,
                lhs,
                rhs,
                collation: None,
            } => {
                let a = self.emit(lhs, dst, false)?;
                let b = self.emit(rhs, dst + 1, false)?;
                if a != b && a != Kind::Null && b != Kind::Null {
                    return None;
                }
                self.program.ops.push(Op::Compare(dst, *op));
                Kind::Bool
            }
            RExpr::Not(inner) => {
                if !boolean(self.emit(inner, dst, false)?) {
                    return None;
                }
                self.program.ops.push(Op::Not(dst));
                Kind::Bool
            }
            RExpr::IsNull { operand, negated } => {
                self.emit(operand, dst, false)?;
                self.program.ops.push(Op::IsNull(dst, *negated));
                Kind::Bool
            }
            RExpr::And(a, b) | RExpr::Or(a, b) => {
                let and = matches!(e, RExpr::And(..));
                let qual = and && filter;
                if !boolean(self.emit(a, dst, qual)?) {
                    return None;
                }
                let branch = self.program.ops.len();
                let test = if qual {
                    Test::NotTrue
                } else if and {
                    Test::False
                } else {
                    Test::True
                };
                self.program.ops.push(Op::Branch(dst, test, 0));
                if !boolean(self.emit(b, if qual { dst } else { dst + 1 }, qual)?) {
                    return None;
                }
                if !qual {
                    self.program
                        .ops
                        .push(if and { Op::And(dst) } else { Op::Or(dst) });
                }
                let end = self.program.ops.len();
                self.program.ops[branch] = Op::Branch(dst, test, end);
                Kind::Bool
            }
            RExpr::Case {
                arms,
                els,
                coerce_decimal: false,
            } => {
                let mut ends = Vec::new();
                let mut kind = Kind::Null;
                for (cond, result) in arms {
                    if !boolean(self.emit(cond, dst, false)?) {
                        return None;
                    }
                    let next = self.program.ops.len();
                    self.program.ops.push(Op::Branch(dst, Test::NotTrue, 0));
                    kind = unify(kind, self.emit(result, dst, false)?)?;
                    ends.push(self.program.ops.len());
                    self.program.ops.push(Op::Jump(0));
                    let target = self.program.ops.len();
                    self.program.ops[next] = Op::Branch(dst, Test::NotTrue, target);
                }
                kind = unify(kind, self.emit(els, dst, false)?)?;
                let target = self.program.ops.len();
                for end in ends {
                    self.program.ops[end] = Op::Jump(target);
                }
                kind
            }
            RExpr::Coalesce {
                args,
                coerce_decimal: false,
            } => {
                let mut ends = Vec::new();
                let mut kind = Kind::Null;
                for arg in args {
                    kind = unify(kind, self.emit(arg, dst, false)?)?;
                    ends.push(self.program.ops.len());
                    self.program.ops.push(Op::Branch(dst, Test::NotNull, 0));
                }
                let target = self.program.ops.len();
                for end in ends {
                    self.program.ops[end] = Op::Branch(dst, Test::NotNull, target);
                }
                kind
            }
            _ => return None,
        };
        (kind != Kind::Other).then_some(kind)
    }
}
fn integer(k: Kind) -> bool {
    matches!(k, Kind::Int | Kind::Null)
}
fn boolean(k: Kind) -> bool {
    matches!(k, Kind::Bool | Kind::Null)
}
fn unify(a: Kind, b: Kind) -> Option<Kind> {
    if a == Kind::Null {
        Some(b)
    } else if b == Kind::Null || a == b {
        Some(a)
    } else {
        None
    }
}
impl Runner {
    pub(crate) fn compile_lane(e: &RExpr, columns: &[ColType], params: &[Value]) -> Option<Self> {
        if !enabled() {
            return None;
        }
        let columns: Vec<Kind> = columns
            .iter()
            .map(|ty| match ty {
                ColType::Scalar(s) if s.is_integer() => Kind::Int,
                ColType::Scalar(s) if s.is_bool() => Kind::Bool,
                _ => Kind::Other,
            })
            .collect();
        let params: Vec<Kind> = params
            .iter()
            .map(|v| match v {
                Value::Int(_) => Kind::Int,
                Value::Bool(_) => Kind::Bool,
                Value::Null => Kind::Null,
                _ => Kind::Other,
            })
            .collect();
        let out = Self::compile(e, &columns, &params, true);
        record_compile(&out);
        out
    }
    fn compile(e: &RExpr, columns: &[Kind], params: &[Kind], filter: bool) -> Option<Self> {
        let mut compiler = Compiler {
            program: Program {
                ops: Vec::new(),
                slots: 0,
            },
            columns,
            params,
        };
        compiler.emit(e, 0, filter)?;
        let slots = vec![Slot::Null; compiler.program.slots];
        compiler.program.ops.shrink_to_fit();
        Some(Self {
            program: compiler.program,
            slots,
        })
    }
    pub(crate) fn eval(&mut self, row: &[Value], env: &EvalEnv, m: &mut Meter) -> Result<Value> {
        COUNTERS.with(|s| {
            let mut v = s.get();
            v.evaluations += 1;
            s.set(v);
        });
        let mut pc = 0;
        while pc < self.program.ops.len() {
            match self.program.ops[pc] {
                Op::Enter(cost) => {
                    m.guard()?;
                    m.charge(cost);
                }
                Op::Constant(d, v) => self.slots[d] = v,
                Op::Column(d, i) => self.slots[d] = Slot::read(&row[i]),
                Op::Param(d, i) => self.slots[d] = Slot::read(&env.params[i]),
                Op::Arithmetic(d, op, ty) => {
                    self.slots[d] = match (self.slots[d], self.slots[d + 1]) {
                        (Slot::Int(a), Slot::Int(b)) => Slot::read(&eval_arith(op, a, b, ty)?),
                        _ => Slot::Null,
                    }
                }
                Op::Negate(d, ty) => {
                    if let Slot::Int(n) = self.slots[d] {
                        let v = n
                            .checked_neg()
                            .filter(|v| ty.in_range(*v))
                            .ok_or_else(|| overflow(ty))?;
                        self.slots[d] = Slot::Int(v);
                    }
                }
                Op::Cast(d, ty) => {
                    self.slots[d] = match self.slots[d] {
                        Slot::Null => Slot::Null,
                        Slot::Int(n) if ty.is_bool() => Slot::Bool(n != 0),
                        Slot::Int(n) if ty.in_range(n) => Slot::Int(n),
                        Slot::Int(_) => return Err(overflow(ty)),
                        Slot::Bool(b) if ty.is_bool() => Slot::Bool(b),
                        Slot::Bool(b) => Slot::Int(i64::from(b)),
                    }
                }
                Op::Compare(d, op) => {
                    // The tree has both decimal-work and varlen-work guards even for fixed scalars.
                    m.guard()?;
                    m.guard()?;
                    let ord = match (self.slots[d], self.slots[d + 1]) {
                        (Slot::Int(a), Slot::Int(b)) => Some(a.cmp(&b)),
                        (Slot::Bool(a), Slot::Bool(b)) => Some(a.cmp(&b)),
                        _ => None,
                    };
                    self.slots[d] = ord.map_or(Slot::Null, |o| {
                        Slot::Bool(match op {
                            CmpOp::Eq => o.is_eq(),
                            CmpOp::Ne => !o.is_eq(),
                            CmpOp::Lt => o.is_lt(),
                            CmpOp::Gt => o.is_gt(),
                            CmpOp::Le => !o.is_gt(),
                            CmpOp::Ge => !o.is_lt(),
                        })
                    });
                }
                Op::Not(d) => {
                    if let Slot::Bool(b) = self.slots[d] {
                        self.slots[d] = Slot::Bool(!b);
                    }
                }
                Op::IsNull(d, neg) => {
                    self.slots[d] = Slot::Bool(matches!(self.slots[d], Slot::Null) != neg)
                }
                Op::And(d) => {
                    self.slots[d] = match (self.slots[d], self.slots[d + 1]) {
                        (Slot::Bool(false), _) | (_, Slot::Bool(false)) => Slot::Bool(false),
                        (Slot::Bool(true), Slot::Bool(true)) => Slot::Bool(true),
                        _ => Slot::Null,
                    }
                }
                Op::Or(d) => {
                    self.slots[d] = match (self.slots[d], self.slots[d + 1]) {
                        (Slot::Bool(true), _) | (_, Slot::Bool(true)) => Slot::Bool(true),
                        (Slot::Bool(false), Slot::Bool(false)) => Slot::Bool(false),
                        _ => Slot::Null,
                    }
                }
                Op::Branch(d, test, target) => {
                    let v = self.slots[d];
                    let yes = match test {
                        Test::False => matches!(v, Slot::Bool(false)),
                        Test::True => v.true_(),
                        Test::NotTrue => !v.true_(),
                        Test::NotNull => !matches!(v, Slot::Null),
                    };
                    if yes {
                        pc = target;
                        continue;
                    }
                }
                Op::Jump(target) => {
                    pc = target;
                    continue;
                }
            }
            pc += 1;
        }
        Ok(self.slots[0].value())
    }
}

/// Programs are compiled per cursor creation, including prepared-plan hits. They are never
/// retained in the plan cache. Synthetic grouped/window rows deliberately use whole-expression
/// fallback until a typed row-layout contract is carried to these sites.
pub(crate) struct Site {
    pub(crate) projections: Vec<Option<Runner>>,
    pub(crate) filter: Option<Runner>,
}
impl Site {
    pub(crate) fn new(plan: &SelectPlan, exec: &Engine, params: &[Value]) -> Self {
        Self::with_filter(plan, exec, params, plan.filter.get(), true)
    }
    pub(crate) fn with_filter(
        plan: &SelectPlan,
        exec: &Engine,
        params: &[Value],
        filter: Option<&RExpr>,
        projections: bool,
    ) -> Self {
        let active = enabled();
        if !active {
            return Self {
                projections: Vec::new(),
                filter: None,
            };
        }
        let mut columns = Vec::new();
        if !plan.is_agg && !plan.has_window {
            for rel in &plan.rels {
                columns.resize(rel.offset + rel.col_count, Kind::Other);
                if rel.srf.is_none() && rel.cte.is_none() && rel.derived.is_none() {
                    if let Some(table) = exec.table_scoped(rel.db.as_deref(), &rel.table_name) {
                        for (i, col) in table.columns.iter().enumerate() {
                            columns[rel.offset + i] = if col.ty.is_integer() {
                                Kind::Int
                            } else if col.ty.is_bool() {
                                Kind::Bool
                            } else {
                                Kind::Other
                            };
                        }
                    }
                }
            }
        }
        let params: Vec<Kind> = params
            .iter()
            .map(|v| match v {
                Value::Int(_) => Kind::Int,
                Value::Bool(_) => Kind::Bool,
                Value::Null => Kind::Null,
                _ => Kind::Other,
            })
            .collect();
        let build = |expr: &RExpr, filter: bool| {
            let out = Runner::compile(expr, &columns, &params, filter);
            record_compile(&out);
            out
        };
        Self {
            projections: if projections {
                plan.projections.iter().map(|e| build(e, false)).collect()
            } else {
                Vec::new()
            },
            filter: filter.and_then(|e| build(e, true)),
        }
    }
    pub(crate) fn project(
        &mut self,
        expressions: &[RExpr],
        row: &[Value],
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<Row> {
        if self.projections.is_empty() {
            return expressions
                .iter()
                .map(|e| e.eval(row, env, meter))
                .collect();
        }
        expressions
            .iter()
            .zip(&mut self.projections)
            .map(|(e, vm)| match vm {
                Some(vm) => vm.eval(row, env, meter),
                None => e.eval(row, env, meter),
            })
            .collect()
    }
}

fn record_compile(out: &Option<Runner>) {
    COUNTERS.with(|s| {
        let mut v = s.get();
        if let Some(r) = out {
            v.compiled += 1;
            v.program_bytes += (r.program.ops.capacity() * std::mem::size_of::<Op>()) as u64;
            v.scratch_bytes += (r.slots.capacity() * std::mem::size_of::<Slot>()) as u64;
        } else {
            v.fallbacks += 1;
        }
        s.set(v);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    const ROOT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../spec/experiments/expression-vm/"
    );
    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{ROOT}{name}")).unwrap()
    }
    fn lines(text: &str) -> impl Iterator<Item = &str> {
        text.lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
    }
    fn result(v: Result<Value>) -> std::result::Result<String, String> {
        v.map(|v| format!("{v:?}")).map_err(|e| format!("{e:?}"))
    }
    #[test]
    fn expression_vm_shared_equivalence() {
        let mut engine = Engine::new();
        crate::execute(
            &mut engine,
            "CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32, p boolean, q boolean)",
        )
        .unwrap();
        let scope = Scope::single(&engine, engine.table("t").unwrap());
        let params = [Value::Int(7), Value::Int(3)];
        let rng = Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: &engine,
            params: &params,
            outer: &[],
            rng: &rng,
            ctes: CteCtx::empty(),
            host_batch: None,
        };
        let rows: Vec<Row> = lines(&fixture("rows.tsv"))
            .map(|line| {
                line.split('\t')
                    .map(|s| match s {
                        "NULL" => Value::Null,
                        "true" => Value::Bool(true),
                        "false" => Value::Bool(false),
                        _ => Value::Int(s.parse().unwrap()),
                    })
                    .collect()
            })
            .collect();
        let mut checks = 0;
        for line in lines(&fixture("cases.tsv")) {
            let parts: Vec<_> = line.split('\t').collect();
            let (name, filter, source) = (parts[0], parts[1] == "filter", parts[2]);
            let parsed = crate::parser::parse_expression(source).unwrap();
            let (expr, _) = resolve(
                &scope,
                &parsed,
                None,
                &mut AggCtx::Forbidden,
                &mut ParamTypes::default(),
            )
            .unwrap();
            let mut program = Runner::compile(
                &expr,
                &[Kind::Int, Kind::Int, Kind::Int, Kind::Bool, Kind::Bool],
                &[Kind::Int, Kind::Int],
                filter,
            );
            assert_eq!(
                program.is_some(),
                !name.starts_with("fallback_"),
                "admission {name}"
            );
            for row in &rows {
                let tree = |m: &mut Meter| {
                    if filter {
                        expr.eval_filter(row, &env, m).map(Value::Bool)
                    } else {
                        expr.eval(row, &env, m)
                    }
                };
                let mut unlimited = Meter::new();
                let _ = tree(&mut unlimited);
                for limit in 0..=unlimited.accrued + 2 {
                    let mut a = Meter::with_limit(limit);
                    let mut b = Meter::with_limit(limit);
                    let expected = result(tree(&mut a));
                    let actual = result(match &mut program {
                        Some(vm) => vm
                            .eval(row, &env, &mut b)
                            .map(|v| if filter { Value::Bool(v.is_true()) } else { v }),
                        None => tree(&mut b),
                    });
                    assert_eq!(actual, expected, "{name} row {row:?} ceiling {limit}");
                    assert_eq!(b.accrued, a.accrued, "{name} row {row:?} ceiling {limit}");
                    checks += 1;
                }
            }
            // Compiler-only timing is isolated from execution and the production core's host seam.
            if std::env::var("JED_EXPR_VM_COMPILE").is_ok() {
                let start = Instant::now();
                for _ in 0..10_000 {
                    std::hint::black_box(Runner::compile(
                        &expr,
                        &[Kind::Int, Kind::Int, Kind::Int, Kind::Bool, Kind::Bool],
                        &[Kind::Int, Kind::Int],
                        filter,
                    ));
                }
                println!(
                    "{{\"core\":\"rust\",\"phase\":\"compile\",\"case\":\"{name}\",\"iterations\":10000,\"elapsed_ns\":{}}}",
                    start.elapsed().as_nanos()
                );
            }
        }
        println!("expression_vm: {checks} shared row/ceiling equivalence checks");
    }
    fn drain(mut rows: crate::Rows, checksum: bool) -> (u64, i64) {
        let mut hash = 0xcbf29ce484222325u64;
        let mut byte = |b: u8| {
            hash = (hash ^ u64::from(b)).wrapping_mul(0x100000001b3);
        };
        for row in &mut rows {
            if checksum {
                for value in row {
                    for b in value.render().bytes() {
                        byte(b);
                    }
                    byte(0);
                }
                byte(b'\n');
            } else {
                for value in row {
                    std::hint::black_box(value);
                }
            }
        }
        rows.error().unwrap();
        (hash, rows.cost())
    }
    #[test]
    fn expression_vm_workloads() {
        let Ok(mode) = std::env::var("JED_EXPR_VM_MODE") else {
            return;
        };
        assert!(mode == "tree" || mode == "vm");
        set_enabled(mode == "vm");
        let storage = std::env::var("JED_EXPR_VM_STORAGE").unwrap_or_else(|_| "memory".to_string());
        assert!(storage == "memory" || storage == "disk");
        let path = (storage == "disk").then(|| {
            std::env::temp_dir().join(format!(
                "jed-expression-vm-{}-{}.jed",
                std::process::id(),
                mode
            ))
        });
        let mut database = crate::Database::create(crate::CreateOptions {
            path: path.clone(),
            ..Default::default()
        })
        .unwrap();
        let mut db = database.session(SessionOptions::default());
        db.execute(
            "CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32, p boolean, q boolean)",
            &[],
        )
        .unwrap();
        db.execute("INSERT INTO t SELECT g, CASE WHEN g % 17 = 0 THEN NULL ELSE g % 97 END, CASE WHEN g % 19 = 0 THEN NULL ELSE g % 13 + 1 END, CASE WHEN g % 23 = 0 THEN NULL ELSE g % 2 = 0 END, CASE WHEN g % 29 = 0 THEN NULL ELSE g % 3 = 0 END FROM generate_series(1, 20000) AS s(g)", &[]).unwrap();
        if let Some(path) = &path {
            drop(db);
            drop(database);
            database = crate::Database::open(path).unwrap();
            db = database.session(SessionOptions::default());
        }
        let filter = std::env::var("JED_EXPR_VM_FILTER").unwrap_or_default();
        for line in lines(&fixture("workloads.tsv")) {
            let p: Vec<_> = line.split('\t').collect();
            let (name, prepared, sql) = (p[0], p[1] == "prepared", p[3]);
            if !filter.is_empty() && !name.contains(&filter) {
                continue;
            }
            let iterations: usize = std::env::var("JED_EXPR_VM_ITERATIONS")
                .ok()
                .map(|s| s.parse().unwrap())
                .unwrap_or_else(|| p[2].parse().unwrap());
            db.set_max_cost(if name == "metered_scan" {
                1_000_000_000
            } else {
                0
            });
            let prep_start = Instant::now();
            let statement = if prepared {
                Some(db.prepare(sql).unwrap())
            } else {
                None
            };
            let preparation_ns = prep_start.elapsed().as_nanos();
            let first_start = Instant::now();
            let rows = match &statement {
                Some(s) => db.query_prepared(s, &[]),
                None => db.query(sql, &[]),
            }
            .unwrap();
            std::hint::black_box(drain(rows, false));
            let first_execution_ns = first_start.elapsed().as_nanos();
            let warmups = if name.starts_with("point_") {
                iterations.min(100)
            } else {
                3
            };
            for _ in 0..warmups {
                let rows = match &statement {
                    Some(s) => db.query_prepared(s, &[]),
                    None => db.query(sql, &[]),
                }
                .unwrap();
                std::hint::black_box(drain(rows, false));
            }
            let rows = match &statement {
                Some(s) => db.query_prepared(s, &[]),
                None => db.query(sql, &[]),
            }
            .unwrap();
            let expected = drain(rows, true);
            reset_stats();
            let start = Instant::now();
            let mut last = (0, 0);
            for _ in 0..iterations {
                let rows = match &statement {
                    Some(s) => db.query_prepared(s, &[]),
                    None => db.query(sql, &[]),
                }
                .unwrap();
                last = std::hint::black_box(drain(rows, false));
            }
            let elapsed = start.elapsed().as_nanos();
            let s = stats();
            let rows = match &statement {
                Some(s) => db.query_prepared(s, &[]),
                None => db.query(sql, &[]),
            }
            .unwrap();
            let final_result = drain(rows, true);
            assert_eq!(expected, final_result);
            assert_eq!(last.1, expected.1);
            println!(
                "{{\"core\":\"rust\",\"mode\":\"{mode}\",\"storage\":\"{storage}\",\"case\":\"{name}\",\"iterations\":{iterations},\"elapsed_ns\":{elapsed},\"checksum\":\"{:016x}\",\"cost\":{},\"compiled\":{},\"fallbacks\":{},\"evaluations\":{},\"program_bytes\":{},\"scratch_bytes\":{},\"preparation_ns\":{preparation_ns},\"first_execution_ns\":{first_execution_ns}}}",
                final_result.0,
                final_result.1,
                s.compiled,
                s.fallbacks,
                s.evaluations,
                s.program_bytes,
                s.scratch_bytes
            );
        }
        set_enabled(false);
        drop(db);
        drop(database);
        if let Some(path) = path {
            std::fs::remove_file(&path).unwrap();
            let lock = std::path::PathBuf::from(format!("{}.lock", path.display()));
            if lock.exists() {
                std::fs::remove_dir_all(lock).unwrap();
            }
        }
    }
}
