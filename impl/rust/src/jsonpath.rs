//! The `jsonpath` type's compiler, canonical renderer, and evaluator (spec/design/jsonpath.md).
//!
//! The compiled surface: lax/strict mode; a path primary (`$`, `@` inside a filter, or a scalar
//! literal) followed by accessor steps (`.key`, `.*`, `[subscripts]`, `[*]`, numeric / `last`
//! indices, `to` slices, `?(filter)`, and the item methods of §3.1); a top-level comparison
//! predicate. Still deferred (a valid-PG path using one → `0A000` at compile): arithmetic,
//! `like_regex` / `starts with` / `is unknown` / `exists` predicates, `$name` variables,
//! non-literal subscripts, `.**`, and the datetime methods. A malformed path is `42601`. The compiled
//! program is a pure function of the source — kept byte-identical cross-core by the conformance suite
//! (CLAUDE.md §5: a hand-written parser, never codegenned).

use crate::cost::Meter;
use crate::costs::COSTS;
use crate::decimal::{self, Decimal};
use crate::error::{EngineError, Result};
use crate::json::{JsonNode, MAX_JSON_DEPTH};
use crate::sqlstate::SqlState;
use crate::types::ScalarType;
use std::borrow::Cow;
use std::collections::HashMap;
use std::rc::Rc;

/// A compiled jsonpath.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct JsonPath {
    pub strict: bool,
    pub body: PathBody,
}

/// A jsonpath body: an accessor path (produces a sequence) or a top-level boolean predicate
/// (`$.a == 1`, for `jsonb_path_match` / `@@`; jsonpath.md §6).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PathBody {
    /// An accessor path → an ordered jsonb-item sequence.
    Path(PathExpr),
    /// A top-level predicate → a single boolean item (or JSON null when unknown).
    Predicate(Box<Pred>),
}

/// A path primary followed by accessor steps — the top-level path body and every comparison
/// operand. A literal primary with no steps is a plain literal operand.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PathExpr {
    pub primary: Primary,
    pub steps: Vec<Step>,
}

/// Where a path starts.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Primary {
    /// `$` — the context item.
    Root,
    /// `@` — the item a filter is testing (filter context only).
    Current,
    /// A scalar literal — a JSON number / string / boolean / null.
    Lit(JsonNode),
}

/// One accessor step.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Step {
    /// `.key` — a member accessor (the key, unescaped).
    Member(String),
    /// `.*` — the wildcard member accessor.
    WildcardMember,
    /// `[s, …]` — one or more subscripts.
    Subscripts(Vec<Subscript>),
    /// `[*]` — the wildcard element accessor.
    WildcardElement,
    /// `?(predicate)` — a filter: keep only the items for which the predicate is TRUE (§4).
    Filter(Box<Pred>),
    /// `.name(…)` — an item method (§3.1).
    Method(Method),
}

/// An item method (jsonpath.md §3.1). The datetime family is still deferred (`0A000`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Method {
    Type,
    Size,
    Double,
    Ceiling,
    Floor,
    Abs,
    KeyValue,
    Bigint,
    Boolean,
    Integer,
    Number,
    String,
    /// `.decimal([precision [, scale]])` — the optional integer arguments, as written.
    Decimal(Option<Decimal>, Option<Decimal>),
}

impl Method {
    /// The method's name as PostgreSQL spells it in an error message (`jspOperationName`).
    fn name(&self) -> &'static str {
        match self {
            Method::Type => "type",
            Method::Size => "size",
            Method::Double => "double",
            Method::Ceiling => "ceiling",
            Method::Floor => "floor",
            Method::Abs => "abs",
            Method::KeyValue => "keyvalue",
            Method::Bigint => "bigint",
            Method::Boolean => "boolean",
            Method::Integer => "integer",
            Method::Number => "number",
            Method::String => "string",
            Method::Decimal(..) => "decimal",
        }
    }
}

/// A filter predicate (jsonpath.md §4, the comparison subset). 3-valued — `Not`/`And`/`Or` follow
/// SQL/JSON's Kleene logic, but a filter keeps an item only when the predicate is definitely TRUE.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Pred {
    Or(Box<Pred>, Box<Pred>),
    And(Box<Pred>, Box<Pred>),
    Not(Box<Pred>),
    /// `lhs cmp rhs` — an existential comparison (true if SOME pair of items compares true).
    Compare(PathExpr, CmpOp, PathExpr),
}

/// A jsonpath comparison operator (`==`, `!=`/`<>`, `<`, `<=`, `>`, `>=`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// One subscript: a single index or an `i to j` slice.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Subscript {
    Index(Index),
    Slice(Index, Index),
}

/// A subscript index: an integer literal (as written — evaluation rejects one outside the integer
/// range) or the `last` sentinel.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Index {
    Number(Decimal),
    Last,
}

/// A jsonpath construct that is valid in PostgreSQL but not yet supported by jed (a deferred
/// follow-on): `0A000`, a documented divergence.
fn unsupported(what: &str) -> EngineError {
    EngineError::new(
        SqlState::FeatureNotSupported,
        format!("jsonpath {what} is not supported yet"),
    )
}

/// A malformed jsonpath literal: `42601` (PostgreSQL's syntax-error class for a bad path literal).
fn malformed(detail: &str) -> EngineError {
    EngineError::new(SqlState::SyntaxError, format!("invalid jsonpath: {detail}"))
}

impl JsonPath {
    /// Compile a jsonpath source string. Malformed → `42601`; a valid-PG but unsupported construct
    /// → `0A000`.
    pub fn compile(src: &str) -> Result<JsonPath> {
        let mut p = Parser {
            s: src.as_bytes(),
            i: 0,
            depth: 0,
            in_filter: 0,
        };
        p.skip_ws();
        // Optional mode word: `strict` / `lax` (default lax).
        let strict = if p.eat_keyword("strict") {
            true
        } else {
            p.eat_keyword("lax");
            false
        };
        p.skip_ws();
        // A parenthesized top-level predicate — `($.a == 1)`, which is also the canonical render of a
        // top-level predicate, so this round-trips render → compile. A parenthesized literal (`(1.5)`,
        // the render of a number literal followed by a step) starts a path instead.
        if p.peek() == Some(b'(') && !p.paren_literal_ahead() {
            let pred = p.parse_pred()?;
            p.skip_ws();
            if p.peek().is_some() {
                return Err(malformed("unexpected trailing input in predicate"));
            }
            return Ok(JsonPath {
                strict,
                body: PathBody::Predicate(Box::new(pred)),
            });
        }
        // Remember the body start: if the path turns out to be the LHS of a TOP-LEVEL predicate
        // (`$.a == 1`, for jsonb_path_match / @@), we re-parse from here as a predicate.
        let body_start = p.i;
        let expr = p.parse_path_expr(true)?;
        p.skip_ws();
        // After the path: a comparison / logical operator makes the whole thing a top-level predicate
        // (re-parse from the body start as a predicate); arithmetic is a deferred follow-on.
        match p.peek() {
            None => Ok(JsonPath {
                strict,
                body: PathBody::Path(expr),
            }),
            Some(b'=' | b'<' | b'>' | b'!' | b'&' | b'|') => {
                p.i = body_start;
                let pred = p.parse_pred()?;
                p.skip_ws();
                if p.peek().is_some() {
                    return Err(malformed("unexpected trailing input in predicate"));
                }
                Ok(JsonPath {
                    strict,
                    body: PathBody::Predicate(Box::new(pred)),
                })
            }
            Some(b'+' | b'-' | b'*' | b'/' | b'%') => Err(unsupported("path arithmetic")),
            // A trailing WORD predicate operator (`like_regex`, `starts with`, `is unknown`) is a
            // top-level predicate too — deferred `0A000` (not malformed). Any other word is malformed.
            Some(c) if c.is_ascii_alphabetic() => {
                let rest = &p.s[p.i..];
                if rest.starts_with(b"like_regex")
                    || rest.starts_with(b"starts")
                    || rest.starts_with(b"is")
                {
                    Err(unsupported("top-level predicate expressions"))
                } else {
                    Err(malformed("unexpected trailing input in path"))
                }
            }
            Some(_) => Err(malformed("unexpected trailing input in path")),
        }
    }

    /// The canonical render (spec/design/jsonpath.md §2): `strict` kept / `lax` omitted; member keys
    /// quoted; `[*]`, `[i]`, `[i to j]` subscripts; methods lowercase; matches PostgreSQL's
    /// `jsonpath_out`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.strict {
            out.push_str("strict ");
        }
        match &self.body {
            PathBody::Path(e) => write_path_expr(e, &mut out),
            // A top-level predicate renders parenthesized (PG's `jsonpath_out`): `($."a" == 1)`.
            PathBody::Predicate(pred) => {
                out.push('(');
                write_pred(pred, &mut out);
                out.push(')');
            }
        }
        out
    }
}

/// Render a path primary and its steps. A number literal followed by a step is parenthesized
/// (`(1.5).type()`), as PostgreSQL prints it.
fn write_path_expr(e: &PathExpr, out: &mut String) {
    match &e.primary {
        Primary::Root => out.push('$'),
        Primary::Current => out.push('@'),
        Primary::Lit(n @ JsonNode::Number(_)) if !e.steps.is_empty() => {
            out.push('(');
            out.push_str(&crate::json::json_compact_out(n));
            out.push(')');
        }
        Primary::Lit(n) => out.push_str(&crate::json::json_compact_out(n)),
    }
    write_steps(&e.steps, out);
}

/// Render an accessor-step sequence.
fn write_steps(steps: &[Step], out: &mut String) {
    for step in steps {
        match step {
            Step::Member(k) => {
                out.push('.');
                write_quoted(k, out);
            }
            Step::WildcardMember => out.push_str(".*"),
            Step::WildcardElement => out.push_str("[*]"),
            Step::Subscripts(subs) => {
                out.push('[');
                for (n, s) in subs.iter().enumerate() {
                    if n > 0 {
                        out.push(',');
                    }
                    match s {
                        Subscript::Index(i) => write_index(i, out),
                        Subscript::Slice(a, b) => {
                            write_index(a, out);
                            out.push_str(" to ");
                            write_index(b, out);
                        }
                    }
                }
                out.push(']');
            }
            Step::Filter(pred) => {
                out.push_str("?(");
                write_pred(pred, out);
                out.push(')');
            }
            Step::Method(m) => {
                out.push('.');
                out.push_str(m.name());
                out.push('(');
                if let Method::Decimal(p, s) = m {
                    if let Some(p) = p {
                        out.push_str(&p.render());
                    }
                    if let Some(s) = s {
                        out.push(',');
                        out.push_str(&s.render());
                    }
                }
                out.push(')');
            }
        }
    }
}

/// Render a filter predicate (PG's `?(…)` form: `&&`/`||` spaced, `!(…)`, `a op b` spaced).
fn write_pred(pred: &Pred, out: &mut String) {
    match pred {
        Pred::Or(a, b) => {
            write_pred(a, out);
            out.push_str(" || ");
            write_pred(b, out);
        }
        Pred::And(a, b) => {
            write_pred(a, out);
            out.push_str(" && ");
            write_pred(b, out);
        }
        Pred::Not(p) => {
            out.push_str("!(");
            write_pred(p, out);
            out.push(')');
        }
        Pred::Compare(l, op, r) => {
            write_path_expr(l, out);
            out.push(' ');
            out.push_str(match op {
                CmpOp::Eq => "==",
                CmpOp::Ne => "!=",
                CmpOp::Lt => "<",
                CmpOp::Le => "<=",
                CmpOp::Gt => ">",
                CmpOp::Ge => ">=",
            });
            out.push(' ');
            write_path_expr(r, out);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Evaluation (jsonpath.md §3-4) — the lax/strict ordered SQL/JSON item sequence.
// ---------------------------------------------------------------------------------------------

/// How a caller consumes the sequence (jsonpath.md §5).
#[derive(Clone, Copy, Debug)]
pub struct EvalOpts {
    /// An existence test (`jsonb_path_exists`, `@?`, `JSON_EXISTS`): a lax path stops at its first
    /// item, so a later item's error is never reached. A strict path evaluates fully (§4.2).
    pub exists: bool,
    /// How many variable base objects the caller supplies (PostgreSQL counts one for the
    /// `jsonb_path_*` functions' `vars` argument, none for the operators and the SQL/JSON query
    /// functions). The first `.keyvalue()` object is numbered one past them (§3.1).
    pub vars: i64,
}

/// Compile a path for evaluation, charging `jsonpath_compile` for each source byte first
/// (jsonpath.md §7). Every evaluation compiles its path; there is no cache.
pub fn compile_metered(src: &str, m: &mut Meter) -> Result<JsonPath> {
    m.charge(COSTS.jsonpath_compile * src.len() as i64);
    m.guard()?;
    JsonPath::compile(src)
}

/// Evaluate a compiled path over a jsonb context item → the ordered SQL/JSON sequence
/// (jsonpath.md §3). Items borrowed from the document stay borrowed; generated items (method
/// results, `.keyvalue()` objects, literals) are owned. Any error propagates.
pub fn eval<'d>(
    path: &JsonPath,
    ctx: &'d JsonNode,
    opts: EvalOpts,
    m: &mut Meter,
) -> Result<Vec<Cow<'d, JsonNode>>> {
    let mut items = Vec::new();
    eval_items(path, ctx, opts, m, &mut items)?;
    Ok(items.iter().map(Item::materialize).collect())
}

/// [`eval`] in PostgreSQL's silent mode (`@?`, `@@`): a SQL/JSON (class `22`) error ends the
/// evaluation, returning the items found before it and `true`. Any other error propagates.
pub fn eval_silent<'d>(
    path: &JsonPath,
    ctx: &'d JsonNode,
    opts: EvalOpts,
    m: &mut Meter,
) -> Result<(Vec<Cow<'d, JsonNode>>, bool)> {
    let mut items = Vec::new();
    let errored = match eval_items(path, ctx, opts, m, &mut items) {
        Ok(()) => false,
        Err(e) if is_soft(&e) => true,
        Err(e) => return Err(e),
    };
    Ok((items.iter().map(Item::materialize).collect(), errored))
}

/// A SQL/JSON (class `22`) error: the kind silent mode and filter operands suppress. A cost,
/// budget, or cancellation error is never suppressed (§7).
fn is_soft(e: &EngineError) -> bool {
    e.code().starts_with("22")
}

fn eval_items<'d>(
    path: &JsonPath,
    ctx: &'d JsonNode,
    opts: EvalOpts,
    m: &mut Meter,
    out: &mut Vec<Item<'d>>,
) -> Result<()> {
    let mut cx = Ctx {
        root: ctx,
        strict: path.strict,
        m,
        next_gen: 1 + opts.vars,
        root_offsets: None,
    };
    match &path.body {
        PathBody::Path(e) => {
            let seed = match &e.primary {
                Primary::Lit(n) => Item::Val(n.clone()),
                _ => Item::Doc(ctx),
            };
            let stop_at_first = opts.exists && !path.strict;
            run_path(&mut cx, &e.steps, seed, Base::Root, stop_at_first, out)
        }
        // A top-level predicate → a single item: its truth value as a boolean, or JSON null when
        // unknown (§4.4).
        PathBody::Predicate(pred) => {
            let truth = eval_pred(&mut cx, pred, &Item::Doc(ctx), &Base::Root)?;
            out.push(Item::Val(truth.map_or(JsonNode::Null, JsonNode::Bool)));
            Ok(())
        }
    }
}

/// One item of the SQL/JSON sequence.
#[derive(Clone, Debug)]
enum Item<'d> {
    /// A node of the context document.
    Doc(&'d JsonNode),
    /// A scalar the evaluation produced: a path literal or an item-method result.
    Val(JsonNode),
    /// A `.keyvalue()` object `{"id": …, "key": …, "value": …}`, materialized only on output.
    Kv(Rc<KvPair<'d>>),
}

/// One `.keyvalue()` pair object (jsonpath.md §3.1).
#[derive(Debug)]
struct KvPair<'d> {
    key: &'d str,
    value: Item<'d>,
    /// The `"id"` member: the source object's id.
    id: i64,
    /// This object's own base-object number — the id prefix of a `.keyvalue()` applied within it.
    generated: i64,
}

impl<'d> Item<'d> {
    /// The document node, borrowed for the document's lifetime.
    fn doc(&self) -> Option<&'d JsonNode> {
        match self {
            Item::Doc(n) => Some(*n),
            _ => None,
        }
    }

    /// The node of a document or generated item (`None` for a `.keyvalue()` object).
    fn node(&self) -> Option<&JsonNode> {
        match self {
            Item::Doc(n) => Some(n),
            Item::Val(n) => Some(n),
            Item::Kv(_) => None,
        }
    }

    fn type_name(&self) -> &'static str {
        match self.node() {
            Some(n) => crate::json::typeof_name(n),
            None => "object",
        }
    }

    /// The item as a jsonb value: a document node stays borrowed; a `.keyvalue()` object is built.
    fn materialize(&self) -> Cow<'d, JsonNode> {
        match self {
            Item::Doc(n) => Cow::Borrowed(*n),
            Item::Val(n) => Cow::Owned(n.clone()),
            Item::Kv(kv) => Cow::Owned(JsonNode::Object(vec![
                ("id".to_string(), JsonNode::Number(Decimal::from_i64(kv.id))),
                ("key".to_string(), JsonNode::String(kv.key.to_string())),
                ("value".to_string(), kv.value.materialize().into_owned()),
            ])),
        }
    }
}

/// The base object a `.keyvalue()` id is measured against (jsonpath.md §3.1): the context item
/// while a `$` path runs, or the `.keyvalue()` object whose continuation is running.
#[derive(Clone, Debug)]
enum Base<'d> {
    Root,
    Kv(Rc<KvPair<'d>>),
}

/// The evaluation context shared by a path and the filter operands nested in it.
struct Ctx<'d, 'm> {
    root: &'d JsonNode,
    strict: bool,
    m: &'m mut Meter,
    /// The next `.keyvalue()` object number, assigned in evaluation order.
    next_gen: i64,
    /// Each document container's byte offset in PostgreSQL's binary jsonb layout, computed on the
    /// first `.keyvalue()` that needs one.
    root_offsets: Option<HashMap<*const JsonNode, i64>>,
}

impl Ctx<'_, '_> {
    /// Charge `n` units of `jsonpath_step` and guard (jsonpath.md §7).
    fn charge(&mut self, n: i64) -> Result<()> {
        self.m.charge(COSTS.jsonpath_step * n);
        self.m.guard()
    }
}

/// A step application in progress: step `step` applied to `item`, producing its outputs lazily so
/// each output's continuation runs before the next output is produced (PostgreSQL's depth-first
/// order, which fixes the error raised first, the early stop of an existence test, and the
/// `.keyvalue()` numbering).
struct Frame<'d> {
    step: usize,
    item: Item<'d>,
    base: Base<'d>,
    /// lax auto-unwrapping applies to this item (§4.1); `false` for the elements of an unwrapped
    /// array, which are not unwrapped again.
    unwrap: bool,
    started: bool,
    /// Output cursors.
    a: usize,
    b: usize,
    /// The current subscript's resolved `(from, to)` element range.
    range: Option<(i64, i64)>,
    /// The `.keyvalue()` id of this frame's object.
    kv_id: i64,
}

impl<'d> Frame<'d> {
    fn new(step: usize, item: Item<'d>, base: Base<'d>, unwrap: bool) -> Frame<'d> {
        Frame {
            step,
            item,
            base,
            unwrap,
            started: false,
            a: 0,
            b: 0,
            range: None,
            kv_id: 0,
        }
    }
}

/// What a frame produces next.
enum Out<'d> {
    /// An item for the following step (or the result), with the base its continuation runs under.
    Item(Item<'d>, Base<'d>),
    /// One element of a lax-unwrapped array, to which this frame's step applies in turn.
    Sub(Frame<'d>),
    Done,
}

/// Run `steps` over `seed` depth-first with an explicit stack (no native recursion per step),
/// appending each final item to `out`. `stop_at_first` ends the run at the first final item.
fn run_path<'d>(
    cx: &mut Ctx<'d, '_>,
    steps: &[Step],
    seed: Item<'d>,
    base: Base<'d>,
    stop_at_first: bool,
    out: &mut Vec<Item<'d>>,
) -> Result<()> {
    if steps.is_empty() {
        out.push(seed);
        return Ok(());
    }
    let unwrap = !cx.strict;
    let mut stack = vec![Frame::new(0, seed, base, unwrap)];
    while let Some(top) = stack.last_mut() {
        match next_out(cx, steps, top)? {
            Out::Item(item, base) => {
                let next = top.step + 1;
                if next == steps.len() {
                    out.push(item);
                    if stop_at_first {
                        return Ok(());
                    }
                } else {
                    stack.push(Frame::new(next, item, base, unwrap));
                }
            }
            Out::Sub(frame) => stack.push(frame),
            Out::Done => {
                stack.pop();
            }
        }
    }
    Ok(())
}

/// Whether lax mode unwraps an array before applying this step (§4.1 rule 1). The subscripts
/// handle arrays themselves, and `.type()` / `.size()` describe the array.
fn unwraps(step: &Step) -> bool {
    match step {
        Step::Member(_) | Step::WildcardMember | Step::Filter(_) => true,
        Step::Subscripts(_) | Step::WildcardElement => false,
        Step::Method(m) => !matches!(m, Method::Type | Method::Size),
    }
}

/// Produce a frame's next output, charging its step application and each emitted item (§7).
fn next_out<'d>(cx: &mut Ctx<'d, '_>, steps: &[Step], f: &mut Frame<'d>) -> Result<Out<'d>> {
    let step = &steps[f.step];
    if f.unwrap
        && unwraps(step)
        && let Some(JsonNode::Array(elems)) = f.item.doc()
    {
        // lax: apply the step to each element instead (one level — an element array is not
        // unwrapped again).
        if f.a < elems.len() {
            let e = &elems[f.a];
            f.a += 1;
            return Ok(Out::Sub(Frame::new(
                f.step,
                Item::Doc(e),
                f.base.clone(),
                false,
            )));
        }
        return Ok(Out::Done);
    }
    let strict = cx.strict;
    let child = match step {
        Step::Member(key) => {
            if f.started {
                return Ok(Out::Done);
            }
            f.started = true;
            cx.charge(1)?;
            member_of(cx, &f.item, key)?
        }
        Step::WildcardMember => {
            if !f.started {
                f.started = true;
                cx.charge(1)?;
                if f.item.type_name() != "object" {
                    if strict {
                        return Err(EngineError::new(
                            SqlState::SqlJsonObjectNotFound,
                            "jsonpath wildcard member accessor can only be applied to an object",
                        ));
                    }
                    return Ok(Out::Done);
                }
            }
            let i = f.a;
            f.a += 1;
            match (&f.item, f.item.doc()) {
                (_, Some(JsonNode::Object(members))) => members.get(i).map(|(_, v)| Item::Doc(v)),
                (Item::Kv(kv), _) => kv_member(kv, i),
                _ => None,
            }
        }
        Step::Subscripts(subs) => {
            if !f.started {
                f.started = true;
                cx.charge(1)?;
                if strict && !matches!(f.item.doc(), Some(JsonNode::Array(_))) {
                    return Err(EngineError::new(
                        SqlState::SqlJsonArrayNotFound,
                        "jsonpath array accessor can only be applied to an array",
                    ));
                }
            }
            // A non-array is a singleton array in lax (§4.1 rule 2).
            let array = match f.item.doc() {
                Some(JsonNode::Array(elems)) => Some(elems),
                _ => None,
            };
            let size = array.map_or(1, |elems| elems.len() as i64);
            loop {
                let to = match f.range {
                    Some((_, to)) => to,
                    None => {
                        let Some(sub) = subs.get(f.a) else {
                            return Ok(Out::Done);
                        };
                        let (from, to) = subscript_range(sub, size, strict)?;
                        f.range = Some((from, to));
                        f.b = from as usize;
                        to
                    }
                };
                if (f.b as i64) <= to {
                    let i = f.b;
                    f.b += 1;
                    break Some(match array {
                        Some(elems) => Item::Doc(&elems[i]),
                        None => f.item.clone(),
                    });
                }
                f.range = None;
                f.a += 1;
            }
        }
        Step::WildcardElement => {
            if !f.started {
                f.started = true;
                cx.charge(1)?;
                if !matches!(f.item.doc(), Some(JsonNode::Array(_))) {
                    if strict {
                        return Err(EngineError::new(
                            SqlState::SqlJsonArrayNotFound,
                            "jsonpath wildcard array accessor can only be applied to an array",
                        ));
                    }
                    // lax: the item is a singleton array.
                    f.a = 1;
                    cx.charge(1)?;
                    return Ok(Out::Item(f.item.clone(), f.base.clone()));
                }
            }
            match f.item.doc() {
                Some(JsonNode::Array(elems)) if f.a < elems.len() => {
                    f.a += 1;
                    Some(Item::Doc(&elems[f.a - 1]))
                }
                _ => None,
            }
        }
        // `?(predicate)` — keep the item when the predicate is definitely TRUE (§4). The predicate's
        // `@` is the item; `$` is the context item.
        Step::Filter(pred) => {
            if f.started {
                return Ok(Out::Done);
            }
            f.started = true;
            cx.charge(1)?;
            if eval_pred(cx, pred, &f.item, &f.base)? == Some(true) {
                Some(f.item.clone())
            } else {
                None
            }
        }
        Step::Method(Method::KeyValue) => {
            if !f.started {
                f.started = true;
                cx.charge(1)?;
                if f.item.type_name() != "object" {
                    return Err(EngineError::new(
                        SqlState::SqlJsonObjectNotFound,
                        "jsonpath item method .keyvalue() can only be applied to an object",
                    ));
                }
                f.kv_id = keyvalue_id(cx, &f.item, &f.base);
            }
            let i = f.a;
            f.a += 1;
            let pair = match (&f.item, f.item.doc()) {
                (_, Some(JsonNode::Object(members))) => {
                    members.get(i).map(|(k, v)| (k.as_str(), Item::Doc(v)))
                }
                (Item::Kv(kv), _) => kv_member(kv, i).map(|v| (KV_KEYS[i], v)),
                _ => None,
            };
            let Some((key, value)) = pair else {
                return Ok(Out::Done);
            };
            let generated = cx.next_gen;
            cx.next_gen += 1;
            let kv = Rc::new(KvPair {
                key,
                value,
                id: f.kv_id,
                generated,
            });
            cx.charge(1)?;
            return Ok(Out::Item(Item::Kv(kv.clone()), Base::Kv(kv)));
        }
        Step::Method(method) => {
            if f.started {
                return Ok(Out::Done);
            }
            f.started = true;
            cx.charge(1)?;
            Some(apply_method(method, &f.item, strict)?)
        }
    };
    match child {
        Some(item) => {
            cx.charge(1)?;
            Ok(Out::Item(item, f.base.clone()))
        }
        None => Ok(Out::Done),
    }
}

/// The member names of a `.keyvalue()` object, in canonical key order.
const KV_KEYS: [&str; 3] = ["id", "key", "value"];

/// The `i`th member value of a `.keyvalue()` object.
fn kv_member<'d>(kv: &KvPair<'d>, i: usize) -> Option<Item<'d>> {
    match i {
        0 => Some(Item::Val(JsonNode::Number(Decimal::from_i64(kv.id)))),
        1 => Some(Item::Val(JsonNode::String(kv.key.to_string()))),
        2 => Some(kv.value.clone()),
        _ => None,
    }
}

/// `.key` on one item (§7 rule 3): one unit per member examined (stored order, up to and including
/// the first match). A missing member or a non-object raises in strict and yields nothing in lax.
fn member_of<'d>(cx: &mut Ctx<'d, '_>, item: &Item<'d>, key: &str) -> Result<Option<Item<'d>>> {
    let found = match (item, item.doc()) {
        (_, Some(JsonNode::Object(members))) => {
            let at = members.iter().position(|(k, _)| k == key);
            cx.charge(at.map_or(members.len(), |i| i + 1) as i64)?;
            at.map(|i| Item::Doc(&members[i].1))
        }
        (Item::Kv(kv), _) => {
            let at = KV_KEYS.iter().position(|k| *k == key);
            cx.charge(at.map_or(KV_KEYS.len(), |i| i + 1) as i64)?;
            at.and_then(|i| kv_member(kv, i))
        }
        _ if cx.strict => {
            return Err(EngineError::new(
                SqlState::SqlJsonMemberNotFound,
                "jsonpath member accessor can only be applied to an object",
            ));
        }
        _ => return Ok(None),
    };
    if found.is_none() && cx.strict {
        return Err(EngineError::new(
            SqlState::SqlJsonMemberNotFound,
            format!("JSON object does not contain key \"{key}\""),
        ));
    }
    Ok(found)
}

/// Resolve one subscript against an array of `size` elements → its inclusive `(from, to)` range,
/// clamped. An index outside the integer range raises in both modes; strict raises on an
/// out-of-bounds or reversed range, which lax just leaves (partly) empty.
fn subscript_range(sub: &Subscript, size: i64, strict: bool) -> Result<(i64, i64)> {
    let resolve = |i: &Index| -> Result<i64> {
        match i {
            Index::Number(n) => n
                .to_i64_round()
                .filter(|v| i32::try_from(*v).is_ok())
                .ok_or_else(|| {
                    EngineError::new(
                        SqlState::InvalidSqlJsonSubscript,
                        "jsonpath array subscript is out of integer range",
                    )
                }),
            Index::Last => Ok(size - 1),
        }
    };
    let (from, to) = match sub {
        Subscript::Index(i) => {
            let v = resolve(i)?;
            (v, v)
        }
        Subscript::Slice(a, b) => (resolve(a)?, resolve(b)?),
    };
    if strict && (from < 0 || from > to || to >= size) {
        return Err(EngineError::new(
            SqlState::InvalidSqlJsonSubscript,
            "jsonpath array subscript is out of bounds",
        ));
    }
    Ok((from.max(0), to.min(size - 1)))
}

/// Evaluate a filter predicate to a Kleene truth value (`Some(true)`/`Some(false)`/`None` = unknown).
/// Both operands of `&&`/`||` are always evaluated (no short-circuit), so their cost is fixed (§7).
fn eval_pred<'d>(
    cx: &mut Ctx<'d, '_>,
    pred: &Pred,
    current: &Item<'d>,
    base: &Base<'d>,
) -> Result<Option<bool>> {
    Ok(match pred {
        Pred::Or(a, b) => {
            let x = eval_pred(cx, a, current, base)?;
            let y = eval_pred(cx, b, current, base)?;
            match (x, y) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            }
        }
        Pred::And(a, b) => {
            let x = eval_pred(cx, a, current, base)?;
            let y = eval_pred(cx, b, current, base)?;
            match (x, y) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            }
        }
        Pred::Not(p) => eval_pred(cx, p, current, base)?.map(|b| !b),
        Pred::Compare(l, op, r) => eval_compare(cx, l, *op, r, current, base)?,
    })
}

/// Existential comparison (§4.4) over every pair `(a in lhs-seq, b in rhs-seq)`, left outer. An
/// operand error → `None` (unknown); the right operand is not evaluated when the left one fails.
/// lax: any true pair → true, else any incomparable pair → unknown, else false (so an empty operand
/// is false). strict: any incomparable pair → unknown, even beside a true pair. Each tested pair
/// charges its unit plus a SQL comparison's size extra (§7 rule 4).
fn eval_compare<'d>(
    cx: &mut Ctx<'d, '_>,
    l: &PathExpr,
    op: CmpOp,
    r: &PathExpr,
    current: &Item<'d>,
    base: &Base<'d>,
) -> Result<Option<bool>> {
    let Some(ls) = eval_operand(cx, l, current, base)? else {
        return Ok(None);
    };
    let Some(rs) = eval_operand(cx, r, current, base)? else {
        return Ok(None);
    };
    let strict = cx.strict;
    let mut found = false;
    let mut any_unknown = false;
    for a in &ls {
        for b in &rs {
            cx.charge(1)?;
            match (a.node(), b.node()) {
                (Some(JsonNode::String(x)), Some(JsonNode::String(y))) => {
                    let w = x.chars().count().min(y.chars().count()).max(1);
                    cx.m.charge(COSTS.varlen_compare * (w as i64 - 1));
                    cx.m.guard()?;
                }
                (Some(JsonNode::Number(x)), Some(JsonNode::Number(y))) => {
                    cx.m.charge(COSTS.decimal_work * (decimal::work_linear(x, y) as i64 - 1));
                    cx.m.guard()?;
                }
                _ => {}
            }
            match compare_items(a, op, b) {
                Some(true) if !strict => return Ok(Some(true)),
                Some(true) => found = true,
                Some(false) => {}
                None if strict => return Ok(None),
                None => any_unknown = true,
            }
        }
    }
    Ok(if found {
        Some(true)
    } else if any_unknown {
        None
    } else {
        Some(false)
    })
}

/// Evaluate a comparison operand to its item sequence (a path, or a singleton literal). `Ok(None)`
/// is a SQL/JSON error, which makes the comparison unknown rather than propagating (§4.2: filter
/// operands never raise). A cost, budget, or cancellation error still propagates (§7).
fn eval_operand<'d>(
    cx: &mut Ctx<'d, '_>,
    e: &PathExpr,
    current: &Item<'d>,
    base: &Base<'d>,
) -> Result<Option<Vec<Item<'d>>>> {
    let (seed, seed_base) = match &e.primary {
        Primary::Root => (Item::Doc(cx.root), Base::Root),
        Primary::Current => (current.clone(), base.clone()),
        Primary::Lit(n) => (Item::Val(n.clone()), base.clone()),
    };
    let mut seq = Vec::new();
    match run_path(cx, &e.steps, seed, seed_base, false, &mut seq) {
        Ok(()) => {}
        Err(err) if is_soft(&err) => return Ok(None),
        Err(err) => return Err(err),
    }
    if cx.strict {
        return Ok(Some(seq));
    }
    // lax: an array-valued operand item is unwrapped ONE level, so the existential comparison ranges
    // over its elements (§4.1 rule 4).
    let mut out = Vec::with_capacity(seq.len());
    for item in seq {
        match item.doc() {
            Some(JsonNode::Array(elems)) => out.extend(elems.iter().map(Item::Doc)),
            _ => out.push(item),
        }
    }
    Ok(Some(out))
}

/// Compare two items under a jsonpath operator (§4.4). Same-type scalars compare by their order
/// (numbers numerically, strings by code point, `false < true`, `null = null`) under every operator.
/// null against any other item is `!=` and nothing else. Any other pair (mixed non-null types, or
/// two arrays/objects) is `None` (unknown). A `.keyvalue()` object is an object.
fn compare_items(a: &Item<'_>, op: CmpOp, b: &Item<'_>) -> Option<bool> {
    use std::cmp::Ordering;
    let (x, y) = match (a.node(), b.node()) {
        (Some(x), Some(y)) => (x, y),
        // An object against null is `!=`; against anything else it is not comparable.
        (Some(JsonNode::Null), None) | (None, Some(JsonNode::Null)) => {
            return Some(op == CmpOp::Ne);
        }
        _ => return None,
    };
    let ord: Ordering = match (x, y) {
        (JsonNode::Number(x), JsonNode::Number(y)) => x.cmp_value(y),
        (JsonNode::String(x), JsonNode::String(y)) => x.cmp(y),
        (JsonNode::Bool(x), JsonNode::Bool(y)) => x.cmp(y),
        (JsonNode::Null, JsonNode::Null) => Ordering::Equal,
        (JsonNode::Null, _) | (_, JsonNode::Null) => return Some(op == CmpOp::Ne),
        _ => return None, // mixed types and containers are not comparable
    };
    Some(match op {
        CmpOp::Eq => ord == Ordering::Equal,
        CmpOp::Ne => ord != Ordering::Equal,
        CmpOp::Lt => ord == Ordering::Less,
        CmpOp::Le => ord != Ordering::Greater,
        CmpOp::Gt => ord == Ordering::Greater,
        CmpOp::Ge => ord != Ordering::Less,
    })
}

// ---------------------------------------------------------------------------------------------
// Item methods (jsonpath.md §3.1) — PostgreSQL's executeItemOptUnwrapTarget method arms.
// ---------------------------------------------------------------------------------------------

/// `22036` non_numeric_sql_json_item — the class every method coercion failure raises.
fn non_numeric(msg: String) -> EngineError {
    EngineError::new(SqlState::NonNumericSqlJsonItem, msg)
}

/// The "can only be applied to …" error of a method applied to the wrong kind of item.
fn wrong_kind(method: &Method, what: &str) -> EngineError {
    non_numeric(format!(
        "jsonpath item method .{}() can only be applied to {what}",
        method.name()
    ))
}

/// The "argument … is invalid for type …" error of a failed conversion.
fn bad_argument(method: &Method, arg: &str, ty: &str) -> EngineError {
    non_numeric(format!(
        "argument \"{arg}\" of jsonpath item method .{}() is invalid for type {ty}",
        method.name()
    ))
}

fn nan_or_infinity(method: &Method) -> EngineError {
    non_numeric(format!(
        "NaN or Infinity is not allowed for jsonpath item method .{}()",
        method.name()
    ))
}

/// Apply a single-result item method (every method except `.keyvalue()`) to one item.
fn apply_method<'d>(method: &Method, item: &Item<'d>, strict: bool) -> Result<Item<'d>> {
    let node = item.node();
    match method {
        Method::Type => Ok(Item::Val(JsonNode::String(item.type_name().to_string()))),
        Method::Size => {
            let n = match node {
                Some(JsonNode::Array(elems)) => elems.len() as i64,
                _ if strict => {
                    return Err(EngineError::new(
                        SqlState::SqlJsonArrayNotFound,
                        "jsonpath item method .size() can only be applied to an array",
                    ));
                }
                // lax: a non-array is a singleton array (§4.1 rule 3).
                _ => 1,
            };
            Ok(Item::Val(JsonNode::Number(Decimal::from_i64(n))))
        }
        Method::Abs | Method::Floor | Method::Ceiling => {
            let Some(JsonNode::Number(d)) = node else {
                return Err(wrong_kind(method, "a numeric value"));
            };
            let v = match method {
                Method::Abs => d.abs(),
                Method::Floor => d.floor()?,
                _ => d.ceil()?,
            };
            Ok(Item::Val(JsonNode::Number(v)))
        }
        // A number is only checked to be a valid double (the item itself is the result); a string
        // is read as a double and converted back the way PostgreSQL's float8_numeric does.
        Method::Double => match node {
            Some(JsonNode::Number(d)) => {
                float8_in(method, &d.render())?;
                Ok(item.clone())
            }
            Some(JsonNode::String(s)) => {
                let v = float8_in(method, s)?;
                Ok(Item::Val(JsonNode::Number(Decimal::from_float64_dbl_dig(
                    v,
                ))))
            }
            _ => Err(wrong_kind(method, "a string or numeric value")),
        },
        Method::Bigint | Method::Integer => {
            let (ty, scalar) = match method {
                Method::Bigint => ("bigint", ScalarType::Int64),
                _ => ("integer", ScalarType::Int32),
            };
            let v = match node {
                // A number rounds half away from zero (numeric → int) and must fit the type.
                Some(JsonNode::Number(d)) => d
                    .to_i64_round()
                    .filter(|v| *v >= scalar.min() && *v <= scalar.max())
                    .ok_or_else(|| bad_argument(method, &d.render(), ty))?,
                Some(JsonNode::String(s)) => crate::executor::parse_int_literal(s, scalar)
                    .map_err(|_| bad_argument(method, s, ty))?,
                _ => return Err(wrong_kind(method, "a string or numeric value")),
            };
            Ok(Item::Val(JsonNode::Number(Decimal::from_i64(v))))
        }
        Method::Boolean => {
            let b = match node {
                Some(JsonNode::Bool(_)) => return Ok(item.clone()),
                // A number must read as an integer (its text through int4 input): nonzero is true.
                Some(JsonNode::Number(d)) => {
                    let text = d.render();
                    crate::executor::parse_int_literal(&text, ScalarType::Int32)
                        .map_err(|_| bad_argument(method, &text, "boolean"))?
                        != 0
                }
                Some(JsonNode::String(s)) => {
                    parse_bool(s).ok_or_else(|| bad_argument(method, s, "boolean"))?
                }
                _ => {
                    return Err(wrong_kind(method, "a boolean, string, or numeric value"));
                }
            };
            Ok(Item::Val(JsonNode::Bool(b)))
        }
        Method::Number | Method::Decimal(..) => {
            let (num, numstr) = match node {
                Some(JsonNode::Number(d)) => (Cow::Borrowed(d), d.render()),
                Some(JsonNode::String(s)) => (Cow::Owned(numeric_in(method, s)?), s.clone()),
                _ => return Err(wrong_kind(method, "a string or numeric value")),
            };
            match method {
                Method::Decimal(Some(p), s) => Ok(Item::Val(JsonNode::Number(apply_typmod(
                    method,
                    &num,
                    &numstr,
                    p,
                    s.as_ref(),
                )?))),
                _ => match num {
                    Cow::Borrowed(_) => Ok(item.clone()),
                    Cow::Owned(d) => Ok(Item::Val(JsonNode::Number(d))),
                },
            }
        }
        Method::String => {
            let s = match node {
                Some(JsonNode::String(_)) => return Ok(item.clone()),
                Some(JsonNode::Number(d)) => d.render(),
                Some(JsonNode::Bool(b)) => if *b { "true" } else { "false" }.to_string(),
                _ => {
                    return Err(wrong_kind(
                        method,
                        "a boolean, string, numeric, or datetime value",
                    ));
                }
            };
            Ok(Item::Val(JsonNode::String(s)))
        }
        Method::KeyValue => unreachable!(".keyvalue() is applied by next_out"),
    }
}

/// PostgreSQL's `float8in` over a method argument: surrounding whitespace, a sign, digits with an
/// optional point and exponent, or the special spellings. A result that overflows binary64, or a
/// nonzero mantissa that underflows to zero, is out of range. NaN and ±Infinity are refused.
fn float8_in(method: &Method, text: &str) -> Result<f64> {
    let t = text.trim_matches(|c: char| c.is_ascii_whitespace());
    let v = crate::executor::parse_f64_literal(t)
        .map_err(|_| bad_argument(method, text, "double precision"))?;
    if v.is_nan() || v.is_infinite() {
        return Err(nan_or_infinity(method));
    }
    let mantissa = t.split(['e', 'E']).next().unwrap_or("");
    if v == 0.0 && mantissa.bytes().any(|b| b.is_ascii_digit() && b != b'0') {
        return Err(bad_argument(method, text, "double precision"));
    }
    Ok(v)
}

/// PostgreSQL's `numeric_in` over a method argument (jed's own decimal grammar, grammar.md §36),
/// refusing NaN and ±Infinity with their own message.
fn numeric_in(method: &Method, text: &str) -> Result<Decimal> {
    if let Ok(d) = crate::executor::parse_decimal_literal(text).and_then(Decimal::check_cap) {
        return Ok(d);
    }
    let word = text
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .to_ascii_lowercase();
    let body = word
        .strip_prefix('+')
        .or_else(|| word.strip_prefix('-'))
        .unwrap_or(&word);
    if word == "nan" || body == "infinity" || body == "inf" {
        return Err(nan_or_infinity(method));
    }
    Err(bad_argument(method, text, "numeric"))
}

/// `.decimal(p [, s])`: round to `numeric(p, s)` (PostgreSQL's typmod rules — precision 1..1000,
/// scale −1000..1000, rounding half away from zero) and require fewer than `p − s` integer digits.
fn apply_typmod(
    method: &Method,
    num: &Decimal,
    numstr: &str,
    p: &Decimal,
    s: Option<&Decimal>,
) -> Result<Decimal> {
    let as_i32 = |d: &Decimal| d.to_i64_round().and_then(|v| i32::try_from(v).ok());
    let precision = as_i32(p).ok_or_else(|| {
        non_numeric(
            "precision of jsonpath item method .decimal() is out of range for type integer"
                .to_string(),
        )
    })?;
    let scale = match s {
        Some(s) => as_i32(s).ok_or_else(|| {
            non_numeric(
                "scale of jsonpath item method .decimal() is out of range for type integer"
                    .to_string(),
            )
        })?,
        None => 0,
    };
    let max = decimal::MAX_PRECISION as i32;
    if !(1..=max).contains(&precision) {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("NUMERIC precision {precision} must be between 1 and {max}"),
        ));
    }
    if !(-max..=max).contains(&scale) {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("NUMERIC scale {scale} must be between {} and {max}", -max),
        ));
    }
    let rounded = num
        .round_places(i64::from(scale))
        .map_err(|_| bad_argument(method, numstr, "numeric"))?;
    // The digits from the point to the most significant nonzero digit must fit `p − s`.
    let int_digits = rounded.precision() as i64 - rounded.scale() as i64;
    if !rounded.is_zero() && int_digits > i64::from(precision - scale) {
        return Err(bad_argument(method, numstr, "numeric"));
    }
    Ok(rounded)
}

/// PostgreSQL's `parse_bool`: case-insensitive `true`/`false`/`yes`/`no` and their prefixes, `on`,
/// `off` (or `of`), `1`, `0` — with no surrounding whitespace.
fn parse_bool(s: &str) -> Option<bool> {
    let lower = s.to_ascii_lowercase();
    let prefix_of = |word: &str| !lower.is_empty() && word.starts_with(lower.as_str());
    match lower.as_bytes().first() {
        Some(b't') if prefix_of("true") => Some(true),
        Some(b'f') if prefix_of("false") => Some(false),
        Some(b'y') if prefix_of("yes") => Some(true),
        Some(b'n') if prefix_of("no") => Some(false),
        Some(b'o') if lower == "on" => Some(true),
        Some(b'o') if lower.len() >= 2 && "off".starts_with(lower.as_str()) => Some(false),
        Some(b'1') if lower.len() == 1 => Some(true),
        Some(b'0') if lower.len() == 1 => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// `.keyvalue()` ids — byte offsets in PostgreSQL's binary jsonb layout (jsonpath.md §3.1).
// ---------------------------------------------------------------------------------------------

/// The id of a `.keyvalue()` object: `10^10 × base number + the object's byte offset inside the base
/// object`, where the base is the context item (number 0) or the `.keyvalue()` object whose
/// continuation is running. PostgreSQL measures the offset in its binary jsonb layout, which is a
/// pure function of the value, so jed computes the same layout arithmetically.
fn keyvalue_id<'d>(cx: &mut Ctx<'d, '_>, item: &Item<'d>, base: &Base<'d>) -> i64 {
    const BASE_STRIDE: i64 = 10_000_000_000;
    match base {
        Base::Root => match item {
            Item::Doc(n) => root_offset(cx, n),
            _ => 0,
        },
        Base::Kv(kv) => {
            let offset = match (item, &kv.value) {
                (Item::Kv(k), _) if Rc::ptr_eq(k, kv) => 0,
                // A container inside the pair's value: the value's offset in the pair object plus
                // the container's offset inside the value (both are 4-byte aligned, so the value's
                // internal layout does not depend on where it sits).
                (Item::Doc(n), Item::Doc(v)) => {
                    let value_at = align4(
                        40 + pg_numeric_size(&Decimal::from_i64(kv.id)) + kv.key.len() as i64,
                    );
                    value_at + root_offset(cx, n) - root_offset(cx, v)
                }
                _ => 0,
            };
            kv.generated * BASE_STRIDE + offset
        }
    }
}

/// A document container's offset in the context item's binary layout.
fn root_offset(cx: &mut Ctx<'_, '_>, n: &JsonNode) -> i64 {
    let root = cx.root;
    let map = cx.root_offsets.get_or_insert_with(|| {
        let mut map = HashMap::new();
        pg_layout(root, 0, &mut map);
        map
    });
    map.get(&(n as *const JsonNode)).copied().unwrap_or(0)
}

fn align4(off: i64) -> i64 {
    (off + 3) & !3
}

/// Lay `n` out at byte offset `off` the way PostgreSQL's `convertJsonbValue` does, recording each
/// container's start, and return the end offset. A container is 4-byte aligned: a header word, one
/// 4-byte entry per element (two per object member — keys, then values), then the keys' bytes and
/// the element/value data. A string is its UTF-8 bytes, a number its aligned `numeric` datum, and
/// `null`/`true`/`false` take no data bytes.
fn pg_layout(n: &JsonNode, off: i64, map: &mut HashMap<*const JsonNode, i64>) -> i64 {
    match n {
        JsonNode::Null | JsonNode::Bool(_) => off,
        JsonNode::String(s) => off + s.len() as i64,
        JsonNode::Number(d) => align4(off) + pg_numeric_size(d),
        JsonNode::Array(elems) => {
            let start = align4(off);
            map.insert(n as *const JsonNode, start);
            let mut p = start + 4 + 4 * elems.len() as i64;
            for e in elems {
                p = pg_layout(e, p, map);
            }
            p
        }
        JsonNode::Object(members) => {
            let start = align4(off);
            map.insert(n as *const JsonNode, start);
            let mut p = start + 4 + 8 * members.len() as i64;
            for (k, _) in members {
                p += k.len() as i64;
            }
            for (_, v) in members {
                p = pg_layout(v, p, map);
            }
            p
        }
    }
}

/// The byte size of PostgreSQL's `numeric` datum for `d`: a 4-byte varlena header, a 2-byte header
/// (short form) or 4 bytes (long form), and one 2-byte base-10000 digit per group from the most to
/// the least significant nonzero group. The short form needs a display scale ≤ 63 and a weight
/// (the leading group's power of 10000) in −64..=63. Zero has no digits.
fn pg_numeric_size(d: &Decimal) -> i64 {
    let text = d.render();
    let text = text.trim_start_matches('-');
    let (int, frac) = text.split_once('.').unwrap_or((text, ""));
    let digits: Vec<u8> = int.bytes().chain(frac.bytes()).collect();
    let Some(first) = digits.iter().position(|&b| b != b'0') else {
        return 6;
    };
    let last = digits.iter().rposition(|&b| b != b'0').unwrap_or(first);
    let power = |i: usize| int.len() as i64 - 1 - i as i64;
    let weight = power(first).div_euclid(4);
    let ndigits = weight - power(last).div_euclid(4) + 1;
    let short = d.scale() <= 63 && (-64..=63).contains(&weight);
    (if short { 6 } else { 8 }) + 2 * ndigits
}

fn write_index(i: &Index, out: &mut String) {
    match i {
        Index::Number(n) => out.push_str(&n.render()),
        Index::Last => out.push_str("last"),
    }
}

/// Render a member key as a canonical jsonpath quoted string (`"…"` with JSON escaping).
fn write_quoted(k: &str, out: &mut String) {
    out.push('"');
    for c in k.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn is_member_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_member_cont(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    /// The program nesting depth (spec/design/json.md §6.4): +1 per filter `?(`, parenthesized
    /// group, `!`, and `&&`/`||` chain step, bounded by [`MAX_JSON_DEPTH`] → `54001`. Bounding it at
    /// the compiler bounds every derived walk (render, evaluate, `Clone`/`Drop`).
    depth: usize,
    /// How many filters enclose the cursor — `@` is valid only inside one.
    in_filter: usize,
}

impl Parser<'_> {
    /// Descend one program nesting level, enforcing [`MAX_JSON_DEPTH`] (json.md §6.4). The caller
    /// restores it on the success path (an error aborts the whole compile).
    fn deepen(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_JSON_DEPTH {
            return Err(EngineError::new(
                SqlState::StatementTooComplex,
                format!("jsonpath nesting depth exceeds the maximum of {MAX_JSON_DEPTH}"),
            ));
        }
        Ok(())
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn peek_at(&self, k: usize) -> Option<u8> {
        self.s.get(self.i + k).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    /// Skip jsonpath whitespace (PostgreSQL's `[ \t\n\r\f]`).
    fn skip_ws(&mut self) {
        while self
            .peek()
            .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c'))
        {
            self.i += 1;
        }
    }

    /// Consume `kw` if it appears as a whole WORD at the cursor — i.e. the following byte is not an
    /// identifier-continuation character (so `last]`, `to `, `strict $` all match, but `lastfoo` does
    /// not).
    fn eat_keyword(&mut self, kw: &str) -> bool {
        let kb = kw.as_bytes();
        if self.s[self.i..].starts_with(kb) {
            let after = self.s.get(self.i + kb.len()).copied();
            if after.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == b'_')) {
                self.i += kb.len();
                return true;
            }
        }
        false
    }

    /// Parse a bare identifier (a member key or method name).
    fn parse_ident(&mut self) -> String {
        let start = self.i;
        while self.peek().is_some_and(is_member_cont) {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
    }

    /// Parse a `"…"` jsonpath string — a port of PostgreSQL's jsonpath scanner escapes
    /// (jsonpath_scan.l, jsonpath.md §2.1): `\b \f \n \r \t \v`; a run of `\uXXXX` / `\u{X…}`
    /// (1–6 hex digits) escapes, within which a surrogate pair combines; `\xHH`; and a backslash
    /// before any other character is that character. A decoded NUL is `22P05`, an unpaired surrogate
    /// `22P02` (PG's Detail), and a malformed `\u` / `\x` or a code point above U+10FFFF `42601`.
    fn parse_quoted(&mut self) -> Result<String> {
        self.i += 1; // opening "
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(malformed("unterminated string")),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => match self.s.get(self.i + 1).copied() {
                    None => return Err(malformed("unexpected end after backslash")),
                    Some(b'u') => self.parse_unicode_run(&mut out)?,
                    Some(b'x') => {
                        let hex = self.s.get(self.i + 2..self.i + 4);
                        let ch = hex
                            .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
                            .and_then(|h| std::str::from_utf8(h).ok())
                            .and_then(|h| u32::from_str_radix(h, 16).ok())
                            .ok_or_else(|| malformed("invalid hexadecimal character sequence"))?;
                        push_code_point(ch, &mut out)?;
                        self.i += 4;
                    }
                    Some(c) => {
                        self.i += 1; // the backslash; the escaped character follows
                        match c {
                            b'b' => out.push('\u{08}'),
                            b'f' => out.push('\u{0c}'),
                            b'n' => out.push('\n'),
                            b'r' => out.push('\r'),
                            b't' => out.push('\t'),
                            b'v' => out.push('\u{0b}'),
                            _ => {
                                self.push_char(&mut out);
                                continue;
                            }
                        }
                        self.i += 1;
                    }
                },
                Some(_) => self.push_char(&mut out),
            }
        }
    }

    /// Copy the one UTF-8 character at the cursor.
    fn push_char(&mut self, out: &mut String) {
        let start = self.i;
        self.i += utf8_len(self.s[self.i]);
        out.push_str(&String::from_utf8_lossy(&self.s[start..self.i]));
    }

    /// A run of consecutive `\u` escapes (the cursor at the first `\`) — PG's `{unicode}+` token,
    /// lexed whole before any is decoded, so a malformed escape anywhere in the run is `42601` ahead
    /// of a decode error. Decoding is PG's `addUnicode`: a high surrogate must be followed, within the
    /// run, by a low one (the pair combines); a lone or doubled surrogate is `22P02`.
    fn parse_unicode_run(&mut self, out: &mut String) -> Result<()> {
        let mut units = Vec::new();
        while self.s.get(self.i) == Some(&b'\\') && self.s.get(self.i + 1) == Some(&b'u') {
            self.i += 2;
            let braced = self.eat(b'{');
            let start = self.i;
            let max = if braced { 6 } else { 4 };
            while self.i - start < max && self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                self.i += 1;
            }
            let digits = self.i - start;
            if (braced && (digits == 0 || !self.eat(b'}'))) || (!braced && digits < 4) {
                return Err(malformed("invalid Unicode escape sequence"));
            }
            let hex =
                std::str::from_utf8(&self.s[start..start + digits]).expect("ASCII hex digits");
            units.push(u32::from_str_radix(hex, 16).expect("1-6 hex digits fit a u32"));
        }
        let mut hi_surrogate: Option<u32> = None;
        for mut ch in units {
            if (0xD800..=0xDBFF).contains(&ch) {
                if hi_surrogate.is_some() {
                    return Err(crate::json::escape_syntax_error(
                        "jsonpath",
                        crate::json::HIGH_SURROGATE_DETAIL,
                    ));
                }
                hi_surrogate = Some(ch);
                continue;
            } else if (0xDC00..=0xDFFF).contains(&ch) {
                let Some(hi) = hi_surrogate.take() else {
                    return Err(crate::json::escape_syntax_error(
                        "jsonpath",
                        crate::json::LOW_SURROGATE_DETAIL,
                    ));
                };
                ch = 0x10000 + (((hi - 0xD800) << 10) | (ch - 0xDC00));
            } else if hi_surrogate.is_some() {
                return Err(crate::json::escape_syntax_error(
                    "jsonpath",
                    crate::json::LOW_SURROGATE_DETAIL,
                ));
            }
            push_code_point(ch, out)?;
        }
        if hi_surrogate.is_some() {
            return Err(crate::json::escape_syntax_error(
                "jsonpath",
                crate::json::LOW_SURROGATE_DETAIL,
            ));
        }
        Ok(())
    }

    /// Parse a `[…]` subscript list (the opening `[` consumed, not the wildcard form). Each subscript
    /// is `index` or `index to index`; `index` is a number or `last`. Anything else → `0A000`.
    fn parse_subscripts(&mut self) -> Result<Vec<Subscript>> {
        let mut subs = Vec::new();
        loop {
            self.skip_ws();
            let a = self.parse_index()?;
            self.skip_ws();
            let sub = if self.eat_keyword("to") {
                self.skip_ws();
                let b = self.parse_index()?;
                self.skip_ws();
                Subscript::Slice(a, b)
            } else {
                Subscript::Index(a)
            };
            subs.push(sub);
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                    continue;
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(subs);
                }
                _ => return Err(malformed("expected `,` or `]` in subscript")),
            }
        }
    }

    fn parse_index(&mut self) -> Result<Index> {
        if self.eat_keyword("last") {
            return Ok(Index::Last);
        }
        match self.peek() {
            // A truncated path (no index where one is required) is malformed.
            None => return Err(malformed("expected a subscript index")),
            // A non-numeric token starts an expression subscript (`$.a`, arithmetic) — a deferred
            // follow-on.
            Some(c) if !(c.is_ascii_digit() || c == b'-') => {
                return Err(unsupported("non-literal subscript expressions"));
            }
            _ => {}
        }
        let neg = self.eat(b'-');
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let digits = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("");
        if digits.is_empty() {
            return Err(malformed("expected digits after `-`"));
        }
        if digits.len() > 1 && digits.starts_with('0') {
            return Err(malformed("trailing junk after numeric literal"));
        }
        Ok(Index::Number(Decimal::from_digits_scale(neg, digits, 0)))
    }

    /// Parse a path primary and its accessor steps. `top` is the root expression, where `@` is not
    /// allowed (PostgreSQL: "@ is not allowed in root expressions").
    fn parse_path_expr(&mut self, top: bool) -> Result<PathExpr> {
        self.skip_ws();
        if let Some(lit) = self.try_paren_literal() {
            return Ok(PathExpr {
                primary: Primary::Lit(lit),
                steps: self.parse_steps()?,
            });
        }
        let primary = match self.peek() {
            Some(b'$') => {
                self.i += 1;
                // `$name` — a path variable — is a deferred follow-on (the `vars` surface).
                if self.peek().is_some_and(|c| is_member_start(c) || c == b'"') {
                    return Err(unsupported("path variables `$name`"));
                }
                Primary::Root
            }
            Some(b'@') => {
                if self.in_filter == 0 {
                    return Err(malformed("@ is not allowed in root expressions"));
                }
                self.i += 1;
                Primary::Current
            }
            Some(b'"') => Primary::Lit(JsonNode::String(self.parse_quoted()?)),
            Some(c) if c.is_ascii_digit() => Primary::Lit(self.parse_number()?),
            Some(b'.') if self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) => {
                Primary::Lit(self.parse_number()?)
            }
            // A negative number. Followed by a step it is PostgreSQL's unary minus applied to the
            // whole path (`-1.5.abs()` is `-(1.5.abs())`) — arithmetic, a deferred follow-on.
            Some(b'-')
                if self
                    .peek_at(1)
                    .is_some_and(|c| c.is_ascii_digit() || c == b'.') =>
            {
                let lit = self.parse_number()?;
                let steps = self.parse_steps()?;
                if !steps.is_empty() {
                    return Err(unsupported("path arithmetic"));
                }
                return Ok(PathExpr {
                    primary: Primary::Lit(lit),
                    steps,
                });
            }
            Some(b'-' | b'+') => return Err(unsupported("path arithmetic")),
            _ => {
                if self.eat_keyword("true") {
                    Primary::Lit(JsonNode::Bool(true))
                } else if self.eat_keyword("false") {
                    Primary::Lit(JsonNode::Bool(false))
                } else if self.eat_keyword("null") {
                    Primary::Lit(JsonNode::Null)
                } else if self.eat_keyword("last") {
                    return Err(malformed("LAST is allowed only in array subscripts"));
                } else if top {
                    // A parenthesized expression, `exists (…)`, or another expression form — a
                    // deferred follow-on.
                    return Err(unsupported("expressions other than an accessor path"));
                } else if self.eat_keyword("exists") {
                    return Err(unsupported("exists predicates"));
                } else {
                    return Err(malformed("expected a comparison operand"));
                }
            }
        };
        Ok(PathExpr {
            primary,
            steps: self.parse_steps()?,
        })
    }

    /// Parse a scalar literal: a string, a number, `true`, `false`, or `null`. `None` (cursor
    /// unmoved) if none starts here.
    fn try_literal(&mut self) -> Option<JsonNode> {
        let start = self.i;
        let lit = match self.peek() {
            Some(b'"') => self.parse_quoted().ok().map(JsonNode::String),
            Some(c) if c.is_ascii_digit() || c == b'-' || c == b'.' => self.parse_number().ok(),
            _ if self.eat_keyword("true") => Some(JsonNode::Bool(true)),
            _ if self.eat_keyword("false") => Some(JsonNode::Bool(false)),
            _ if self.eat_keyword("null") => Some(JsonNode::Null),
            _ => None,
        };
        if lit.is_none() {
            self.i = start;
        }
        lit
    }

    /// Parse a parenthesized literal — `(1.5)`, PostgreSQL's render of a number literal followed by
    /// a step. `None` (cursor unmoved) if the parentheses hold anything else.
    fn try_paren_literal(&mut self) -> Option<JsonNode> {
        let start = self.i;
        if self.eat(b'(') {
            self.skip_ws();
            if let Some(lit) = self.try_literal() {
                self.skip_ws();
                if self.eat(b')') {
                    return Some(lit);
                }
            }
        }
        self.i = start;
        None
    }

    /// Whether a parenthesized literal starts at the cursor.
    fn paren_literal_ahead(&mut self) -> bool {
        let start = self.i;
        let found = self.try_paren_literal().is_some();
        self.i = start;
        found
    }

    /// Parse a number literal (PostgreSQL's jsonpath scanner: an integer, `1.5`, `1.`, `.5`, an
    /// exponent) → a `Number` node. A number running into a letter or digit is trailing junk.
    fn parse_number(&mut self) -> Result<JsonNode> {
        let neg = self.eat(b'-');
        let int_start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let int_part = std::str::from_utf8(&self.s[int_start..self.i]).unwrap_or("");
        if int_part.len() > 1 && int_part.starts_with('0') {
            return Err(malformed("trailing junk after numeric literal"));
        }
        let mut frac = "";
        if self.peek() == Some(b'.') {
            match self.peek_at(1) {
                Some(c) if c.is_ascii_digit() => {
                    self.i += 1;
                    let start = self.i;
                    while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                        self.i += 1;
                    }
                    frac = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("");
                }
                Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                    return Err(malformed("trailing junk after numeric literal"));
                }
                // `1.` — a number with an empty fraction — unless a step follows the point.
                Some(b'*' | b'"' | b'[' | b'.') => {}
                _ if !int_part.is_empty() => self.i += 1,
                _ => {}
            }
        }
        let mut exp = None;
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            let eneg = if self.eat(b'-') {
                true
            } else {
                self.eat(b'+');
                false
            };
            let start = self.i;
            let mut v: i64 = 0;
            while let Some(c) = self.peek().filter(|c| c.is_ascii_digit()) {
                if v < decimal::EXP_LIMIT {
                    v = (v * 10 + i64::from(c - b'0')).min(decimal::EXP_LIMIT);
                }
                self.i += 1;
            }
            if self.i == start {
                return Err(malformed("trailing junk after numeric literal"));
            }
            exp = Some(if eneg { -v } else { v });
        }
        if self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            return Err(malformed("trailing junk after numeric literal"));
        }
        if int_part.is_empty() && frac.is_empty() {
            return Err(malformed("invalid number literal"));
        }
        let (digits, scale) = decimal::decimal_from_parts(int_part, frac, exp);
        let d = Decimal::from_digits_scale(neg, &digits, scale).check_cap()?;
        Ok(JsonNode::Number(d))
    }

    /// Parse a sequence of accessor steps (`.key`, `.*`, `[subscripts]`, `[*]`, `?(filter)`,
    /// `.method(…)`), stopping at the first non-accessor byte (EOF, a comparison/logical operator,
    /// `)`, etc).
    fn parse_steps(&mut self) -> Result<Vec<Step>> {
        let mut steps = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'.') => {
                    self.i += 1;
                    self.skip_ws();
                    if self.eat(b'*') {
                        steps.push(Step::WildcardMember);
                    } else if self.peek() == Some(b'"') {
                        steps.push(Step::Member(self.parse_quoted()?));
                    } else if self.peek().is_some_and(is_member_start) {
                        let name = self.parse_ident();
                        let save = self.i;
                        self.skip_ws();
                        if self.peek() == Some(b'(') {
                            steps.push(Step::Method(self.parse_method(&name)?));
                        } else {
                            self.i = save;
                            steps.push(Step::Member(name));
                        }
                    } else {
                        return Err(malformed("expected a member name after `.`"));
                    }
                }
                Some(b'[') => {
                    self.i += 1;
                    self.skip_ws();
                    if self.eat(b'*') {
                        self.skip_ws();
                        if !self.eat(b']') {
                            return Err(malformed("expected `]` after `[*`"));
                        }
                        steps.push(Step::WildcardElement);
                    } else {
                        steps.push(Step::Subscripts(self.parse_subscripts()?));
                    }
                }
                Some(b'?') => {
                    self.i += 1;
                    self.skip_ws();
                    if !self.eat(b'(') {
                        return Err(malformed("expected `(` after `?`"));
                    }
                    self.deepen()?;
                    self.in_filter += 1;
                    let pred = self.parse_pred()?;
                    self.in_filter -= 1;
                    self.depth -= 1;
                    self.skip_ws();
                    if !self.eat(b')') {
                        return Err(malformed("expected `)` after a filter predicate"));
                    }
                    steps.push(Step::Filter(Box::new(pred)));
                }
                _ => break,
            }
        }
        Ok(steps)
    }

    /// Parse an item method's argument list (the cursor is at its `(`). Method names are matched
    /// case-insensitively, like PostgreSQL's keywords.
    fn parse_method(&mut self, name: &str) -> Result<Method> {
        self.i += 1; // (
        self.skip_ws();
        let method = match name.to_ascii_lowercase().as_str() {
            "type" => Method::Type,
            "size" => Method::Size,
            "double" => Method::Double,
            "ceiling" => Method::Ceiling,
            "floor" => Method::Floor,
            "abs" => Method::Abs,
            "keyvalue" => Method::KeyValue,
            "bigint" => Method::Bigint,
            "boolean" => Method::Boolean,
            "integer" => Method::Integer,
            "number" => Method::Number,
            "string" => Method::String,
            "decimal" => {
                let mut args = Vec::new();
                if !self.eat(b')') {
                    loop {
                        args.push(self.parse_int_arg()?);
                        self.skip_ws();
                        if self.eat(b')') {
                            break;
                        }
                        if !self.eat(b',') {
                            return Err(malformed("expected `,` or `)` in .decimal() arguments"));
                        }
                    }
                }
                if args.len() > 2 {
                    return Err(malformed(
                        ".decimal() can only have an optional precision[,scale]",
                    ));
                }
                let mut it = args.into_iter();
                return Ok(Method::Decimal(it.next(), it.next()));
            }
            "datetime" | "date" | "time" | "time_tz" | "timestamp" | "timestamp_tz" => {
                return Err(unsupported(&format!(
                    "item method .{}()",
                    name.to_ascii_lowercase()
                )));
            }
            _ => return Err(malformed(&format!("unknown item method .{name}()"))),
        };
        if !self.eat(b')') {
            return Err(malformed(&format!(
                "item method .{}() takes no arguments",
                method.name()
            )));
        }
        Ok(method)
    }

    /// Parse a `.decimal()` argument: an optionally signed integer literal.
    fn parse_int_arg(&mut self) -> Result<Decimal> {
        self.skip_ws();
        let neg = if self.eat(b'-') {
            true
        } else {
            self.eat(b'+');
            false
        };
        self.skip_ws();
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let digits = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("");
        if digits.is_empty()
            || (digits.len() > 1 && digits.starts_with('0'))
            || self
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'.')
        {
            return Err(malformed("expected an integer .decimal() argument"));
        }
        Ok(Decimal::from_digits_scale(neg, digits, 0))
    }

    /// Parse a filter predicate (the comparison subset): `||` over `&&` over `!` / `(…)` / comparison.
    fn parse_pred(&mut self) -> Result<Pred> {
        let mut left = self.parse_and()?;
        // Each `||` step deepens the left-deep tree by one level (json.md §6.4).
        let mut steps = 0;
        loop {
            self.skip_ws();
            if self.eat_op(b"||") {
                self.deepen()?;
                steps += 1;
                let right = self.parse_and()?;
                left = Pred::Or(Box::new(left), Box::new(right));
            } else {
                self.depth -= steps;
                return Ok(left);
            }
        }
    }

    fn parse_and(&mut self) -> Result<Pred> {
        let mut left = self.parse_not()?;
        // Each `&&` step deepens the left-deep tree by one level (json.md §6.4).
        let mut steps = 0;
        loop {
            self.skip_ws();
            if self.eat_op(b"&&") {
                self.deepen()?;
                steps += 1;
                let right = self.parse_not()?;
                left = Pred::And(Box::new(left), Box::new(right));
            } else {
                self.depth -= steps;
                return Ok(left);
            }
        }
    }

    fn parse_not(&mut self) -> Result<Pred> {
        self.skip_ws();
        if self.eat(b'!') {
            self.skip_ws();
            if !self.eat(b'(') {
                return Err(malformed("expected `(` after `!`"));
            }
            self.deepen()?;
            let inner = self.parse_pred()?;
            self.depth -= 1;
            self.skip_ws();
            if !self.eat(b')') {
                return Err(malformed("expected `)` after `!(`"));
            }
            return Ok(Pred::Not(Box::new(inner)));
        }
        if self.peek() == Some(b'(') && !self.paren_literal_ahead() {
            self.i += 1;
            self.deepen()?;
            let inner = self.parse_pred()?;
            self.depth -= 1;
            self.skip_ws();
            if !self.eat(b')') {
                return Err(malformed("expected `)` in predicate"));
            }
            return Ok(inner);
        }
        self.parse_comparison()
    }

    /// `operand cmp operand` — the only leaf predicate this slice (`exists` / `like_regex` /
    /// `starts with` / `is unknown` are a follow-on).
    fn parse_comparison(&mut self) -> Result<Pred> {
        let left = self.parse_path_expr(false)?;
        self.skip_ws();
        let op = if self.eat_op(b"==") {
            CmpOp::Eq
        } else if self.eat_op(b"!=") || self.eat_op(b"<>") {
            CmpOp::Ne
        } else if self.eat_op(b"<=") {
            CmpOp::Le
        } else if self.eat_op(b">=") {
            CmpOp::Ge
        } else if self.eat(b'<') {
            CmpOp::Lt
        } else if self.eat(b'>') {
            CmpOp::Gt
        } else {
            return Err(unsupported(
                "filter predicates other than a comparison (exists / like_regex / starts with)",
            ));
        };
        let right = self.parse_path_expr(false)?;
        Ok(Pred::Compare(left, op, right))
    }

    /// Consume a multi-byte operator token if it appears at the cursor.
    fn eat_op(&mut self, op: &[u8]) -> bool {
        if self.s[self.i..].starts_with(op) {
            self.i += op.len();
            true
        } else {
            false
        }
    }
}

/// Append a decoded escape's code point — PG's `addUnicodeChar`: NUL is `22P05` (text cannot hold
/// it), a value above U+10FFFF `42601` ("invalid Unicode code point").
fn push_code_point(ch: u32, out: &mut String) -> Result<()> {
    if ch == 0 {
        return Err(crate::json::unicode_zero_error());
    }
    match char::from_u32(ch) {
        Some(c) => {
            out.push(c);
            Ok(())
        }
        None => Err(EngineError::new(
            SqlState::SyntaxError,
            "invalid Unicode code point",
        )),
    }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}
