//! The `jsonpath` type's compiler, canonical renderer, and evaluator (spec/design/jsonpath.md).
//!
//! The compiled surface: lax/strict mode; a primary (`$`, `@` inside a filter, a scalar literal,
//! `last` inside a subscript, or a parenthesized expression or predicate) followed by accessor
//! steps (`.key`, `.*`, `.**`, `[subscripts]` of any expression, `[*]`, `to` slices, `?(filter)`,
//! and the item methods of §3.1); arithmetic (`+ - * / %`, unary `+ -`, §3.2); a top-level
//! predicate (comparison, `exists`, `like_regex`, `starts with`, `is unknown`, §4.4). Path
//! variables (`$name`, `$"name"`) are bound by the caller's [`Vars`]. The datetime methods and the
//! `like_regex` `m` flag are `0A000` (jsonpath.md §3.1, §4.3). A malformed path is `42601`. The
//! compiled program is a pure function of the source — kept byte-identical cross-core by the
//! conformance suite (CLAUDE.md §5: a hand-written parser, never codegenned).

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
    /// The instruction count of every `like_regex` program in the path, which a metered compile
    /// charges as `regex_compile` (jsonpath.md §7).
    regex_insts: i64,
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
    /// `last` — the index of the innermost subscripted array's last element (subscript expressions
    /// only).
    Last,
    /// `$name` / `$"name"` — a path variable, bound by the caller's [`Vars`] (§5).
    Var(String),
    /// `+e` / `-e` — unary arithmetic over each item of `e` (§3.2).
    Unary(UnaryOp, Box<PathExpr>),
    /// `l op r` — binary arithmetic over two singleton numbers (§3.2).
    Binary(ArithOp, Box<PathExpr>, Box<PathExpr>),
    /// `(predicate)` followed by accessors — the predicate's truth value as an item (`true`,
    /// `false`, or `null` when unknown).
    Pred(Box<Pred>),
}

/// A unary arithmetic operator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnaryOp {
    Plus,
    Minus,
}

/// A binary arithmetic operator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl ArithOp {
    /// The operator as written and as PostgreSQL names it in an error message.
    fn symbol(self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
            ArithOp::Mod => "%",
        }
    }

    /// PostgreSQL's `operationPriority`: `+ -` bind looser than `* / %`.
    fn priority(self) -> u8 {
        match self {
            ArithOp::Add | ArithOp::Sub => 3,
            _ => 4,
        }
    }
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
    /// `.**{first to last}` — the item and its descendants within the level bounds (§3.3); a bound
    /// of `last` is [`LEVEL_LAST`].
    Any { first: u32, last: u32 },
}

/// A `.**` level bound of `last`: PostgreSQL stores it as the largest `uint32`.
pub const LEVEL_LAST: u32 = u32::MAX;

/// An item method (jsonpath.md §3.1). The datetime family is not supported (`0A000`), by decision.
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
    /// `exists (e)` — `e`'s sequence is non-empty.
    Exists(PathExpr),
    /// `(p) is unknown`.
    IsUnknown(Box<Pred>),
    /// `lhs starts with rhs` — `rhs` a string literal or a variable.
    StartsWith(PathExpr, PathExpr),
    /// `lhs like_regex "pattern" flag "flags"`.
    LikeRegex(PathExpr, Box<LikeRegex>),
}

/// A `like_regex` predicate's pattern as written, its flag bits, and its compiled program.
#[derive(Clone, Debug)]
pub struct LikeRegex {
    pub pattern: String,
    pub flags: u8,
    program: Rc<crate::regex::Program>,
}

/// The program is a pure function of the pattern and flags, so they decide equality.
impl PartialEq for LikeRegex {
    fn eq(&self, other: &LikeRegex) -> bool {
        self.pattern == other.pattern && self.flags == other.flags
    }
}

impl Eq for LikeRegex {}

/// `like_regex` flag bits, in PostgreSQL's print order (jsonpath.md §2.3).
const REGEX_I: u8 = 1 << 0;
const REGEX_S: u8 = 1 << 1;
const REGEX_M: u8 = 1 << 2;
const REGEX_X: u8 = 1 << 3;
const REGEX_Q: u8 = 1 << 4;

/// The flag characters in bit order.
const REGEX_FLAG_CHARS: &[u8; 5] = b"ismxq";

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

/// A subscript index: a number literal (as written — evaluation truncates it and rejects one
/// outside the integer range), the `last` sentinel, or any other expression (§3.2).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Index {
    Number(Decimal),
    Last,
    Expr(Box<PathExpr>),
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
        let path = JsonPath::parse(src)?;
        // A path is evaluated from its canonical text, whose parenthesization can nest deeper than
        // the source (`1 + 1 + 1` renders `((1 + 1) + 1)`), so that text must compile too (§8).
        JsonPath::parse(&path.render())?;
        Ok(path)
    }

    /// Parse a jsonpath source string (see [`JsonPath::compile`]).
    fn parse(src: &str) -> Result<JsonPath> {
        let mut p = Parser {
            s: src.as_bytes(),
            i: 0,
            depth: 0,
            in_filter: 0,
            in_subscript: 0,
            regex_insts: 0,
        };
        p.skip_ws()?;
        // Nothing but whitespace and comments: PostgreSQL's grammar yields no path, `22P02`.
        if p.peek().is_none() {
            return Err(EngineError::new(
                SqlState::InvalidTextRepresentation,
                format!("invalid input syntax for type jsonpath: \"{src}\""),
            ));
        }
        // Optional mode word: `strict` / `lax` (default lax).
        let strict = if p.eat_keyword("strict") {
            true
        } else {
            p.eat_keyword("lax");
            false
        };
        // The body is an expression (an accessor path, arithmetic) or a top-level predicate
        // (`$.a == 1`, for jsonb_path_match / @@) — PostgreSQL's `expr_or_predicate`.
        let body = match p.parse_or()? {
            Parsed::Expr(e) => PathBody::Path(e),
            Parsed::Pred(pred) | Parsed::ParenPred(pred) => PathBody::Predicate(Box::new(pred)),
        };
        p.skip_ws()?;
        if p.peek().is_some() {
            return Err(malformed("unexpected trailing input"));
        }
        Ok(JsonPath {
            strict,
            body,
            regex_insts: p.regex_insts,
        })
    }

    /// The canonical render (spec/design/jsonpath.md §2.2): `strict` kept / `lax` omitted; member
    /// keys quoted; `[*]`, `[i]`, `[i to j]` subscripts; methods lowercase; operators spaced and
    /// parenthesized by PostgreSQL's `jsonpath_out` rules.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.strict {
            out.push_str("strict ");
        }
        // The top level parenthesizes an operator (`($."a" + 1)`, `($."a" == 1)`), as PostgreSQL
        // prints it.
        match &self.body {
            PathBody::Path(e) => write_expr(e, true, &mut out),
            PathBody::Predicate(pred) => write_pred(pred, true, &mut out),
        }
        out
    }
}

/// PostgreSQL's `operationPriority` of an expression's head: `||` 0, `&&` 1, comparisons 2, `+ -`
/// 3, `* / %` 4, unary `+ -` 5, anything else 6.
fn expr_priority(e: &PathExpr) -> u8 {
    match &e.primary {
        Primary::Unary(..) => 5,
        Primary::Binary(op, ..) => op.priority(),
        Primary::Pred(pred) => pred_priority(pred),
        _ => 6,
    }
}

fn pred_priority(pred: &Pred) -> u8 {
    match pred {
        Pred::Or(..) => 0,
        Pred::And(..) => 1,
        Pred::Compare(..) | Pred::StartsWith(..) => 2,
        Pred::Not(_) | Pred::Exists(_) | Pred::IsUnknown(_) | Pred::LikeRegex(..) => 6,
    }
}

/// Render an expression — PostgreSQL's `printJsonPathItem`. An operator is parenthesized when
/// `brackets` is set: at the top level, and for an operand that binds no tighter than its parent
/// (`((1 + 2) + 3)`, `(1 - (2 - 3))`). jed also parenthesizes an operator followed by accessors,
/// which PostgreSQL prints bare in an operand (`$."a" + 1.type()`), a text that does not read back
/// as the same path (§2.2). A number literal followed by a step is parenthesized (`(1.5).type()`).
fn write_expr(e: &PathExpr, brackets: bool, out: &mut String) {
    let brackets = brackets || !e.steps.is_empty();
    match &e.primary {
        Primary::Root => out.push('$'),
        Primary::Current => out.push('@'),
        Primary::Last => out.push_str("last"),
        Primary::Var(name) => {
            out.push('$');
            write_quoted(name, out);
        }
        Primary::Lit(n @ JsonNode::Number(_)) if !e.steps.is_empty() => {
            out.push('(');
            out.push_str(&crate::json::json_compact_out(n));
            out.push(')');
        }
        Primary::Lit(n) => out.push_str(&crate::json::json_compact_out(n)),
        Primary::Unary(op, arg) => {
            if brackets {
                out.push('(');
            }
            out.push(match op {
                UnaryOp::Plus => '+',
                UnaryOp::Minus => '-',
            });
            write_expr(arg, expr_priority(arg) <= 5, out);
            if brackets {
                out.push(')');
            }
        }
        Primary::Binary(op, l, r) => {
            if brackets {
                out.push('(');
            }
            write_expr(l, expr_priority(l) <= op.priority(), out);
            out.push(' ');
            out.push_str(op.symbol());
            out.push(' ');
            write_expr(r, expr_priority(r) <= op.priority(), out);
            if brackets {
                out.push(')');
            }
        }
        // A predicate that prints without its own brackets (`!(…)`, `exists (…)`, `(…) is unknown`)
        // keeps the parentheses that make it an operand of the accessors after it (jsonpath.md
        // §2.3).
        Primary::Pred(pred) => match **pred {
            Pred::Not(_) | Pred::Exists(_) | Pred::IsUnknown(_) => {
                out.push('(');
                write_pred(pred, false, out);
                out.push(')');
            }
            _ => write_pred(pred, brackets, out),
        },
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
                write_pred(pred, false, out);
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
            Step::Any { first, last } => write_any_levels(*first, *last, out),
        }
    }
}

/// Render a `.**` step as PostgreSQL's `printJsonPathItem` does: `{0 to last}` is bare, equal
/// bounds print once.
fn write_any_levels(first: u32, last: u32, out: &mut String) {
    let level = |v: u32| {
        if v == LEVEL_LAST {
            "last".to_string()
        } else {
            v.to_string()
        }
    };
    out.push_str(".**");
    if first == 0 && last == LEVEL_LAST {
        return;
    }
    if first == last {
        out.push_str(&format!("{{{}}}", level(first)));
    } else {
        out.push_str(&format!("{{{} to {}}}", level(first), level(last)));
    }
}

/// Render a predicate (PostgreSQL's form: `&&`/`||` and `a op b` spaced, `!(…)`), parenthesized
/// when `brackets` is set and its operands by priority, like [`write_expr`].
fn write_pred(pred: &Pred, brackets: bool, out: &mut String) {
    match pred {
        Pred::Or(a, b) | Pred::And(a, b) => {
            let prio = pred_priority(pred);
            if brackets {
                out.push('(');
            }
            write_pred(a, pred_priority(a) <= prio, out);
            out.push_str(if prio == 0 { " || " } else { " && " });
            write_pred(b, pred_priority(b) <= prio, out);
            if brackets {
                out.push(')');
            }
        }
        Pred::Not(p) => {
            out.push_str("!(");
            write_pred(p, false, out);
            out.push(')');
        }
        Pred::Compare(l, op, r) => {
            if brackets {
                out.push('(');
            }
            write_expr(l, expr_priority(l) <= 2, out);
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
            write_expr(r, expr_priority(r) <= 2, out);
            if brackets {
                out.push(')');
            }
        }
        Pred::Exists(e) => {
            out.push_str("exists (");
            write_expr(e, false, out);
            out.push(')');
        }
        Pred::IsUnknown(p) => {
            out.push('(');
            write_pred(p, false, out);
            out.push_str(") is unknown");
        }
        Pred::StartsWith(l, r) => {
            if brackets {
                out.push('(');
            }
            write_expr(l, expr_priority(l) <= 2, out);
            out.push_str(" starts with ");
            write_expr(r, expr_priority(r) <= 2, out);
            if brackets {
                out.push(')');
            }
        }
        Pred::LikeRegex(l, re) => {
            // PostgreSQL brackets the operand by priority against like_regex's 6, so an operator
            // operand is always wrapped.
            if brackets {
                out.push('(');
            }
            write_expr(l, true, out);
            out.push_str(" like_regex ");
            write_quoted(&re.pattern, out);
            if re.flags != 0 {
                out.push_str(" flag \"");
                for (i, &c) in REGEX_FLAG_CHARS.iter().enumerate() {
                    if re.flags & (1 << i) != 0 {
                        out.push(c as char);
                    }
                }
                out.push('"');
            }
            if brackets {
                out.push(')');
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Evaluation (jsonpath.md §3-4) — the lax/strict ordered SQL/JSON item sequence.
// ---------------------------------------------------------------------------------------------

/// How a caller consumes the sequence (jsonpath.md §5).
#[derive(Clone, Copy, Debug)]
pub struct EvalOpts<'d> {
    /// An existence test (`jsonb_path_exists`, `@?`, `JSON_EXISTS`): a lax path stops at its first
    /// item, so a later item's error is never reached. A strict path evaluates fully (§4.2).
    pub exists: bool,
    /// The variables the path's `$name` references read (§5).
    pub vars: Vars<'d>,
}

/// The variables a path's `$name` references are bound to (jsonpath.md §5), and the variable base
/// objects they count for `.keyvalue()` ids (§3.1).
#[derive(Clone, Copy, Debug)]
pub enum Vars<'d> {
    /// No variables: the `@?` / `@@` operators. Counts no base object.
    None,
    /// The `jsonb_path_*` functions' `vars` object (already checked to be an object by
    /// [`check_vars_object`]). It is one base object, number 1.
    Object(&'d JsonNode),
    /// The SQL/JSON functions' `PASSING` values, as [`passing_vars`] orders them. Each is a base
    /// object numbered by its position in the clause; `count` is the clause's length.
    Passing { vars: &'d [PassingVar], count: i64 },
}

impl Vars<'_> {
    /// How many variable base objects the caller supplies (PostgreSQL's `countVars`): the first
    /// `.keyvalue()` object is numbered one past them.
    fn count(&self) -> i64 {
        match self {
            Vars::None => 0,
            Vars::Object(_) => 1,
            Vars::Passing { count, .. } => *count,
        }
    }
}

/// One `PASSING value AS name` binding (jsonpath.md §5): the value as a `jsonb` item and its base
/// object number (its 1-based position in the clause).
#[derive(Clone, Debug)]
pub struct PassingVar {
    pub name: String,
    pub value: JsonNode,
    pub id: i64,
}

/// Order a `PASSING` clause's bindings for lookup: sorted by name, and of two bindings with the same
/// name only the first kept, as PostgreSQL's lookup finds the first. Each keeps its clause position
/// as its base object number.
pub fn passing_vars(bindings: Vec<(String, JsonNode)>) -> Vec<PassingVar> {
    let mut vars: Vec<PassingVar> = bindings
        .into_iter()
        .enumerate()
        .map(|(i, (name, value))| PassingVar {
            name,
            value,
            id: i as i64 + 1,
        })
        .collect();
    // A stable sort keeps the first of equal names first.
    vars.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    vars.dedup_by(|later, first| later.name == first.name);
    vars
}

/// Check a `jsonb_path_*` function's `vars` argument (PostgreSQL's `countVariablesFromJsonb`): it
/// must be an object, else `22023`. Checked before evaluation, so it is raised even when the path
/// uses no variable and even in silent mode.
pub fn check_vars_object(vars: &JsonNode) -> Result<()> {
    if matches!(vars, JsonNode::Object(_)) {
        return Ok(());
    }
    Err(EngineError::new(
        SqlState::InvalidParameterValue,
        "\"vars\" argument is not an object",
    )
    .with_detail(Some(
        "Jsonpath parameters should be encoded as key-value pairs of \"vars\" object.".to_string(),
    )))
}

/// Compile a path for evaluation, charging `jsonpath_compile` for each source byte first
/// (jsonpath.md §7). Every evaluation compiles its path; there is no cache.
pub fn compile_metered(src: &str, m: &mut Meter) -> Result<JsonPath> {
    m.charge(COSTS.jsonpath_compile * src.len() as i64);
    m.guard()?;
    // The text is a jsonpath value's canonical text (or a JSON_TABLE path), already checked by
    // `compile` at resolve.
    let path = JsonPath::parse(src)?;
    // Each like_regex pattern was compiled with the path (jsonpath.md §7).
    m.charge(COSTS.regex_compile * path.regex_insts);
    m.guard()?;
    Ok(path)
}

/// Evaluate a compiled path over a jsonb context item → the ordered SQL/JSON sequence
/// (jsonpath.md §3). Items borrowed from the document stay borrowed; generated items (method
/// results, `.keyvalue()` objects, literals) are owned. Any error propagates.
pub fn eval<'d>(
    path: &JsonPath,
    ctx: &'d JsonNode,
    opts: EvalOpts<'d>,
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
    opts: EvalOpts<'d>,
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
    opts: EvalOpts<'d>,
    m: &mut Meter,
    out: &mut Vec<Item<'d>>,
) -> Result<()> {
    let mut cx = Ctx {
        root: ctx,
        vars: opts.vars,
        strict: path.strict,
        ignore: false,
        m,
        next_gen: 1 + opts.vars.count(),
        offsets: HashMap::new(),
        laid_out: Vec::new(),
    };
    match &path.body {
        // `@` is not allowed outside a filter, so the root expression's current item is unused.
        PathBody::Path(e) => {
            let stop_at_first = opts.exists && !path.strict;
            run_expr(
                &mut cx,
                e,
                &Item::Doc(ctx),
                &Base::Root,
                None,
                stop_at_first,
                out,
            )
        }
        // A top-level predicate → a single item: its truth value as a boolean, or JSON null when
        // unknown (§4.4).
        PathBody::Predicate(pred) => {
            let truth = eval_pred(&mut cx, pred, &Item::Doc(ctx), &Base::Root, None)?;
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
/// while a `$` path runs, the variable base object while a `$name` path runs, or the `.keyvalue()`
/// object whose continuation is running.
#[derive(Clone, Debug)]
enum Base<'d> {
    Root,
    /// A variable's base object: the `vars` object (number 1) or a `PASSING` value (its position).
    Var {
        tree: &'d JsonNode,
        id: i64,
    },
    Kv(Rc<KvPair<'d>>),
}

/// The evaluation context shared by a path and the filter operands nested in it.
struct Ctx<'d, 'm> {
    root: &'d JsonNode,
    vars: Vars<'d>,
    strict: bool,
    /// Suppresses navigation errors while the rest of a path below `.**` runs, in either mode
    /// (jsonpath.md §3.3) — PostgreSQL's `ignoreStructuralErrors`, which lax always sets.
    ignore: bool,
    m: &'m mut Meter,
    /// The next `.keyvalue()` object number, assigned in evaluation order.
    next_gen: i64,
    /// Each container's byte offset in PostgreSQL's binary jsonb layout of its base object (the
    /// document or a variable's), computed for a base object on the first `.keyvalue()` that needs
    /// one.
    offsets: HashMap<*const JsonNode, i64>,
    /// The base objects laid out into `offsets` so far.
    laid_out: Vec<*const JsonNode>,
}

impl Ctx<'_, '_> {
    /// Charge `n` units of `jsonpath_step` and guard (jsonpath.md §7).
    fn charge(&mut self, n: i64) -> Result<()> {
        self.m.charge(COSTS.jsonpath_step * n);
        self.m.guard()
    }

    /// Whether a navigation failure raises: strict, and not below a `.**` (§3.3, §4.2).
    fn structural(&self) -> bool {
        self.strict && !self.ignore
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
    /// The size of the innermost array being subscripted, which `last` refers to (in a filter of
    /// this step) — set by an enclosing subscript step, and for as long as its outputs' continuations
    /// run, as in PostgreSQL.
    last: Option<i64>,
    /// A subscript step's array size: its own outputs' `last`.
    size: i64,
    /// This frame runs below a `.**`, suppressing navigation errors (§3.3).
    ignore: bool,
    /// A `.**` step's descent: the containers being walked, innermost last.
    walk: Vec<AnyWalk<'d>>,
}

/// One container a `.**` step is walking: its children are at `level`, and `next` is the next
/// child's position.
struct AnyWalk<'d> {
    item: Item<'d>,
    level: u32,
    next: usize,
}

impl<'d> Frame<'d> {
    fn new(
        step: usize,
        item: Item<'d>,
        base: Base<'d>,
        unwrap: bool,
        last: Option<i64>,
        ignore: bool,
    ) -> Frame<'d> {
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
            last,
            size: 0,
            ignore,
            walk: Vec::new(),
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

/// Run an expression depth-first (§3): its head produces items one at a time (§3.2), and each runs
/// through the accessor steps on an explicit stack (no native recursion per step) before the head
/// produces the next. `current` is the item `@` names and `last` the size of the innermost array
/// being subscripted. Each final item is appended to `out`; `stop_at_first` ends the run at the first
/// one — PostgreSQL's lax existence test, which also lets a unary operator skip a non-number.
fn run_expr<'d>(
    cx: &mut Ctx<'d, '_>,
    e: &PathExpr,
    current: &Item<'d>,
    base: &Base<'d>,
    last: Option<i64>,
    stop_at_first: bool,
    out: &mut Vec<Item<'d>>,
) -> Result<()> {
    let steps = &e.steps;
    let unwrap = !cx.strict;
    let mut head = Head {
        started: false,
        items: Vec::new(),
        at: 0,
    };
    let mut stack: Vec<Frame<'d>> = Vec::new();
    loop {
        let Some(top) = stack.last_mut() else {
            let Some((item, item_base)) =
                head_next(cx, e, &mut head, current, base, last, stop_at_first)?
            else {
                return Ok(());
            };
            if steps.is_empty() {
                out.push(item);
                if stop_at_first {
                    return Ok(());
                }
            } else {
                stack.push(Frame::new(0, item, item_base, unwrap, last, cx.ignore));
            }
            continue;
        };
        // The frame's step runs with its own suppression of navigation errors, which reaches any
        // filter operand it evaluates (§3.3).
        let saved = cx.ignore;
        cx.ignore = top.ignore;
        let produced = next_out(cx, steps, current, top);
        cx.ignore = saved;
        match produced? {
            Out::Item(item, base) => {
                let next = top.step + 1;
                if next == steps.len() {
                    out.push(item);
                    if stop_at_first {
                        return Ok(());
                    }
                } else {
                    // A subscript step's outputs run their continuations with `last` naming its
                    // array.
                    let last = match &steps[top.step] {
                        Step::Subscripts(_) => Some(top.size),
                        _ => top.last,
                    };
                    // Every item a `.**` emits runs the rest of the path with navigation errors
                    // suppressed.
                    let ignore = top.ignore || matches!(steps[top.step], Step::Any { .. });
                    stack.push(Frame::new(next, item, base, unwrap, last, ignore));
                }
            }
            Out::Sub(frame) => stack.push(frame),
            Out::Done => {
                stack.pop();
            }
        }
    }
}

/// The state of an expression's head: whether it has started, and a unary operator's operand items
/// with the next one's position.
struct Head<'d> {
    started: bool,
    items: Vec<Item<'d>>,
    at: usize,
}

/// Produce the next item of an expression's head (`$`, `@`, a literal, `last`, an operator, or a
/// parenthesized predicate), with the base its continuation runs under.
fn head_next<'d>(
    cx: &mut Ctx<'d, '_>,
    e: &PathExpr,
    head: &mut Head<'d>,
    current: &Item<'d>,
    base: &Base<'d>,
    last: Option<i64>,
    stop_at_first: bool,
) -> Result<Option<(Item<'d>, Base<'d>)>> {
    if let Primary::Unary(op, arg) = &e.primary {
        // Each item of the operand (lax: an array item unwrapped one level) must be a number: one
        // application each, one emitted item for each number. A lax existence test with nothing
        // after the operator skips a non-number, as PostgreSQL does.
        if !head.started {
            head.started = true;
            head.items = eval_seq(cx, arg, current, base, last, true)?;
        }
        while head.at < head.items.len() {
            let item = head.items[head.at].clone();
            head.at += 1;
            cx.charge(1)?;
            let negated = match item.node() {
                Some(JsonNode::Number(d)) => Some(d.neg()),
                _ => None,
            };
            let Some(negated) = negated else {
                if stop_at_first && e.steps.is_empty() {
                    continue;
                }
                return Err(EngineError::new(
                    SqlState::SqlJsonNumberNotFound,
                    format!(
                        "operand of unary jsonpath operator {} is not a numeric value",
                        match op {
                            UnaryOp::Plus => "+",
                            UnaryOp::Minus => "-",
                        }
                    ),
                ));
            };
            cx.charge(1)?;
            let result = match op {
                UnaryOp::Plus => item,
                UnaryOp::Minus => Item::Val(JsonNode::Number(negated)),
            };
            return Ok(Some((result, base.clone())));
        }
        return Ok(None);
    }
    if head.started {
        return Ok(None);
    }
    head.started = true;
    Ok(Some(match &e.primary {
        Primary::Root => (Item::Doc(cx.root), Base::Root),
        Primary::Current => (current.clone(), base.clone()),
        Primary::Lit(n) => (Item::Val(n.clone()), base.clone()),
        Primary::Var(name) => variable(cx, name)?,
        Primary::Last => (
            Item::Val(JsonNode::Number(Decimal::from_i64(last.unwrap_or(0) - 1))),
            base.clone(),
        ),
        Primary::Binary(op, l, r) => (
            eval_binary(cx, *op, l, r, current, base, last)?,
            base.clone(),
        ),
        // `(predicate)` followed by accessors: the truth value as an item (JSON null for unknown).
        Primary::Pred(pred) => {
            let truth = eval_pred(cx, pred, current, base, last)?;
            (
                Item::Val(truth.map_or(JsonNode::Null, JsonNode::Bool)),
                base.clone(),
            )
        }
        Primary::Unary(..) => unreachable!("a unary head is handled above"),
    }))
}

/// Evaluate an expression to its whole item sequence. With `unwrap` in lax mode an array item is
/// unwrapped one level (§4.1 rule 4) — free: it only changes which items the caller sees.
fn eval_seq<'d>(
    cx: &mut Ctx<'d, '_>,
    e: &PathExpr,
    current: &Item<'d>,
    base: &Base<'d>,
    last: Option<i64>,
    unwrap: bool,
) -> Result<Vec<Item<'d>>> {
    let mut seq = Vec::new();
    run_expr(cx, e, current, base, last, false, &mut seq)?;
    if cx.strict || !unwrap {
        return Ok(seq);
    }
    let mut out = Vec::with_capacity(seq.len());
    for item in seq {
        match item.doc() {
            Some(JsonNode::Array(elems)) => out.extend(elems.iter().map(Item::Doc)),
            _ => out.push(item),
        }
    }
    Ok(out)
}

/// `l op r` (§3.2): both operands are evaluated (lax unwraps an array item one level), then each
/// must be a single number (`22038`). One application, the size-scaled `decimal_work` of the SQL
/// operator, and one emitted item; the arithmetic is `decimal`'s, so `/` takes PostgreSQL's
/// `select_div_scale` and a zero divisor raises `22012`.
fn eval_binary<'d>(
    cx: &mut Ctx<'d, '_>,
    op: ArithOp,
    l: &PathExpr,
    r: &PathExpr,
    current: &Item<'d>,
    base: &Base<'d>,
    last: Option<i64>,
) -> Result<Item<'d>> {
    let ls = eval_seq(cx, l, current, base, last, true)?;
    let rs = eval_seq(cx, r, current, base, last, true)?;
    cx.charge(1)?;
    let single = |seq: &[Item<'d>], side: &str| -> Result<Decimal> {
        match seq {
            [item] => match item.node() {
                Some(JsonNode::Number(d)) => Ok(d.clone()),
                _ => Err(not_single_numeric(side, op)),
            },
            _ => Err(not_single_numeric(side, op)),
        }
    };
    let a = single(&ls, "left")?;
    let b = single(&rs, "right")?;
    let work = match op {
        ArithOp::Add | ArithOp::Sub => decimal::work_linear(&a, &b),
        ArithOp::Mul => decimal::work_mul(&a, &b),
        ArithOp::Div => decimal::work_div(&a, &b),
        ArithOp::Mod => decimal::work_mod(&a, &b),
    };
    cx.m.charge(COSTS.decimal_work * (work as i64 - 1));
    cx.m.guard()?;
    let v = match op {
        ArithOp::Add => a.add(&b)?,
        ArithOp::Sub => a.sub(&b)?,
        ArithOp::Mul => a.mul(&b)?,
        ArithOp::Div => a.div(&b)?,
        ArithOp::Mod => a.rem(&b)?,
    };
    cx.charge(1)?;
    Ok(Item::Val(JsonNode::Number(v)))
}

/// `22038`: a binary operator's operand is not a single number.
fn not_single_numeric(side: &str, op: ArithOp) -> EngineError {
    EngineError::new(
        SqlState::SingletonSqlJsonItemRequired,
        format!(
            "{side} operand of jsonpath operator {} is not a single numeric value",
            op.symbol()
        ),
    )
}

/// Whether lax mode unwraps an array before applying this step (§4.1 rule 1). The subscripts
/// handle arrays themselves, and `.type()` / `.size()` describe the array.
fn unwraps(step: &Step) -> bool {
    match step {
        Step::Member(_) | Step::WildcardMember | Step::Filter(_) => true,
        Step::Subscripts(_) | Step::WildcardElement | Step::Any { .. } => false,
        Step::Method(m) => !matches!(m, Method::Type | Method::Size),
    }
}

/// Produce a frame's next output, charging its step application and each emitted item (§7).
fn next_out<'d>(
    cx: &mut Ctx<'d, '_>,
    steps: &[Step],
    current: &Item<'d>,
    f: &mut Frame<'d>,
) -> Result<Out<'d>> {
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
                f.last,
                f.ignore,
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
                    if cx.structural() {
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
                    if cx.structural() {
                        return Err(EngineError::new(
                            SqlState::SqlJsonArrayNotFound,
                            "jsonpath array accessor can only be applied to an array",
                        ));
                    }
                    // Strict below `.**`: no singleton, no item (§3.3).
                    return Ok(Out::Done);
                }
            }
            // A non-array is a singleton array in lax (§4.1 rule 2).
            let array = match f.item.doc() {
                Some(JsonNode::Array(elems)) => Some(elems),
                _ => None,
            };
            let size = array.map_or(1, |elems| elems.len() as i64);
            f.size = size;
            loop {
                let to = match f.range {
                    Some((_, to)) => to,
                    None => {
                        let Some(sub) = subs.get(f.a) else {
                            return Ok(Out::Done);
                        };
                        let (from, to) = subscript_range(cx, sub, size, current, &f.base)?;
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
                    if cx.structural() {
                        return Err(EngineError::new(
                            SqlState::SqlJsonArrayNotFound,
                            "jsonpath wildcard array accessor can only be applied to an array",
                        ));
                    }
                    if strict {
                        // Strict below `.**`: no singleton, no item (§3.3).
                        return Ok(Out::Done);
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
            if eval_pred(cx, pred, &f.item, &f.base, f.last)? == Some(true) {
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
        Step::Any { first, last } => return next_any(cx, *first, *last, f),
        Step::Method(method) => {
            if f.started {
                return Ok(Out::Done);
            }
            f.started = true;
            cx.charge(1)?;
            if matches!(method, Method::Size)
                && strict
                && !cx.structural()
                && !matches!(f.item.doc(), Some(JsonNode::Array(_)))
            {
                // Strict `.size()` of a non-array below `.**`: no item (§3.3).
                return Ok(Out::Done);
            }
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

/// Produce a `.**` frame's next output (jsonpath.md §3.3): the item itself when the first level is
/// 0, then its descendants in preorder — a child is emitted (when its level is in bounds) before
/// the walk descends into it. Applying the step charges 1, each descendant visited 1, and each
/// emitted item 1 (§7 rule 8).
fn next_any<'d>(cx: &mut Ctx<'d, '_>, first: u32, last: u32, f: &mut Frame<'d>) -> Result<Out<'d>> {
    if !f.started {
        f.started = true;
        cx.charge(1)?;
        if last >= 1 && is_container(&f.item) {
            f.walk.push(AnyWalk {
                item: f.item.clone(),
                level: 1,
                next: 0,
            });
        }
        if first == 0 {
            cx.charge(1)?;
            return Ok(Out::Item(f.item.clone(), f.base.clone()));
        }
    }
    while let Some(top) = f.walk.last_mut() {
        let Some(child) = child_at(&top.item, top.next) else {
            f.walk.pop();
            continue;
        };
        top.next += 1;
        let level = top.level;
        cx.charge(1)?;
        let container = is_container(&child);
        if level < last && container {
            f.walk.push(AnyWalk {
                item: child.clone(),
                level: level + 1,
                next: 0,
            });
        }
        // `{last}` (both bounds the largest level) is PostgreSQL's leaves-only walk.
        if level >= first || (first == LEVEL_LAST && last == LEVEL_LAST && !container) {
            cx.charge(1)?;
            return Ok(Out::Item(child, f.base.clone()));
        }
    }
    Ok(Out::Done)
}

/// Whether an item has children for `.**`: a document array or object, or a `.keyvalue()` object.
fn is_container(item: &Item<'_>) -> bool {
    matches!(
        item,
        Item::Kv(_) | Item::Doc(JsonNode::Array(_) | JsonNode::Object(_))
    )
}

/// A container's `i`th child in stored order: an element, a member value, or a `.keyvalue()`
/// object's id, key, value.
fn child_at<'d>(item: &Item<'d>, i: usize) -> Option<Item<'d>> {
    match item {
        Item::Kv(kv) => kv_member(kv, i),
        Item::Doc(JsonNode::Array(elems)) => elems.get(i).map(Item::Doc),
        Item::Doc(JsonNode::Object(members)) => members.get(i).map(|(_, v)| Item::Doc(v)),
        _ => None,
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
        _ if cx.structural() => {
            return Err(EngineError::new(
                SqlState::SqlJsonMemberNotFound,
                "jsonpath member accessor can only be applied to an object",
            ));
        }
        _ => return Ok(None),
    };
    if found.is_none() && cx.structural() {
        return Err(EngineError::new(
            SqlState::SqlJsonMemberNotFound,
            format!("JSON object does not contain key \"{key}\""),
        ));
    }
    Ok(found)
}

/// Resolve one subscript against an array of `size` elements → its inclusive `(from, to)` range,
/// clamped. An index is truncated toward zero; one outside the integer range raises in both modes,
/// and strict raises on an out-of-bounds or reversed range, which lax just leaves (partly) empty.
/// A non-literal index (`last - 1`, `$.i`) is evaluated with `last` naming this array and must be a
/// single number (`22033`).
fn subscript_range<'d>(
    cx: &mut Ctx<'d, '_>,
    sub: &Subscript,
    size: i64,
    current: &Item<'d>,
    base: &Base<'d>,
) -> Result<(i64, i64)> {
    let (from, to) = match sub {
        Subscript::Index(i) => {
            let v = index_value(cx, i, size, current, base)?;
            (v, v)
        }
        Subscript::Slice(a, b) => (
            index_value(cx, a, size, current, base)?,
            index_value(cx, b, size, current, base)?,
        ),
    };
    if cx.structural() && (from < 0 || from > to || to >= size) {
        return Err(EngineError::new(
            SqlState::InvalidSqlJsonSubscript,
            "jsonpath array subscript is out of bounds",
        ));
    }
    Ok((from.max(0), to.min(size - 1)))
}

/// One subscript index's value (PostgreSQL's `getArrayIndex`).
fn index_value<'d>(
    cx: &mut Ctx<'d, '_>,
    i: &Index,
    size: i64,
    current: &Item<'d>,
    base: &Base<'d>,
) -> Result<i64> {
    let n = match i {
        Index::Number(n) => Cow::Borrowed(n),
        Index::Last => return Ok(size - 1),
        Index::Expr(e) => {
            let seq = eval_seq(cx, e, current, base, Some(size), false)?;
            match seq.as_slice() {
                [item] => match item.node() {
                    Some(JsonNode::Number(n)) => Cow::Owned(n.clone()),
                    _ => return Err(not_single_subscript()),
                },
                _ => return Err(not_single_subscript()),
            }
        }
    };
    n.trunc_places(0)
        .to_i64_round()
        .filter(|v| i32::try_from(*v).is_ok())
        .ok_or_else(|| {
            EngineError::new(
                SqlState::InvalidSqlJsonSubscript,
                "jsonpath array subscript is out of integer range",
            )
        })
}

fn not_single_subscript() -> EngineError {
    EngineError::new(
        SqlState::InvalidSqlJsonSubscript,
        "jsonpath array subscript is not a single numeric value",
    )
}

/// Evaluate a filter predicate to a Kleene truth value (`Some(true)`/`Some(false)`/`None` = unknown).
/// Both operands of `&&`/`||` are always evaluated (no short-circuit), so their cost is fixed (§7).
fn eval_pred<'d>(
    cx: &mut Ctx<'d, '_>,
    pred: &Pred,
    current: &Item<'d>,
    base: &Base<'d>,
    last: Option<i64>,
) -> Result<Option<bool>> {
    Ok(match pred {
        Pred::Or(a, b) => {
            let x = eval_pred(cx, a, current, base, last)?;
            let y = eval_pred(cx, b, current, base, last)?;
            match (x, y) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            }
        }
        Pred::And(a, b) => {
            let x = eval_pred(cx, a, current, base, last)?;
            let y = eval_pred(cx, b, current, base, last)?;
            match (x, y) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            }
        }
        Pred::Not(p) => eval_pred(cx, p, current, base, last)?.map(|b| !b),
        Pred::Compare(l, op, r) => eval_compare(cx, l, *op, r, current, base, last)?,
        Pred::IsUnknown(p) => Some(eval_pred(cx, p, current, base, last)?.is_none()),
        Pred::Exists(e) => eval_exists(cx, e, current, base, last)?,
        Pred::StartsWith(l, r) => eval_starts_with(cx, l, r, current, base, last)?,
        Pred::LikeRegex(l, re) => eval_like_regex(cx, l, re, current, base, last)?,
    })
}

/// `exists (e)` (§4.4 rule 6): true when `e`'s sequence is non-empty, unknown when evaluating it
/// raises a SQL/JSON error. Lax stops at the first item; strict evaluates them all.
fn eval_exists<'d>(
    cx: &mut Ctx<'d, '_>,
    e: &PathExpr,
    current: &Item<'d>,
    base: &Base<'d>,
    last: Option<i64>,
) -> Result<Option<bool>> {
    let mut seq = Vec::new();
    let stop_at_first = !cx.strict;
    match run_expr(cx, e, current, base, last, stop_at_first, &mut seq) {
        Ok(()) => Ok(Some(!seq.is_empty())),
        Err(err) if is_soft(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

/// Combine an existential predicate's `n` pair results in order (§4.4 rule 3): lax is true at the
/// first true pair, else unknown if any pair was, else false; strict is unknown at the first
/// unknown pair, else true if any pair was, else false. A pair test charges its own work.
fn combine_pairs<'d, 'm>(
    cx: &mut Ctx<'d, 'm>,
    n: usize,
    mut test: impl FnMut(&mut Ctx<'d, 'm>, usize) -> Result<Option<bool>>,
) -> Result<Option<bool>> {
    let strict = cx.strict;
    let mut found = false;
    let mut any_unknown = false;
    for k in 0..n {
        match test(cx, k)? {
            None if strict => return Ok(None),
            None => any_unknown = true,
            Some(true) if !strict => return Ok(Some(true)),
            Some(true) => found = true,
            Some(false) => {}
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

/// Charge a SQL comparison's size extra for two strings: `varlen_compare` per code point of the
/// shorter one beyond the first (§7 rule 4).
fn charge_string_compare(cx: &mut Ctx<'_, '_>, a: &str, b: &str) -> Result<()> {
    let w = a.chars().count().min(b.chars().count()).max(1);
    cx.m.charge(COSTS.varlen_compare * (w as i64 - 1));
    cx.m.guard()
}

/// `l starts with r` (§4.4 rule 5): `l`'s items (lax-unwrapped) against `r`'s (not unwrapped); a
/// pair of strings is true when the left begins with the right, any other pair unknown.
fn eval_starts_with<'d>(
    cx: &mut Ctx<'d, '_>,
    l: &PathExpr,
    r: &PathExpr,
    current: &Item<'d>,
    base: &Base<'d>,
    last: Option<i64>,
) -> Result<Option<bool>> {
    let Some(ls) = eval_operand(cx, l, current, base, last)? else {
        return Ok(None);
    };
    let rs = match eval_seq(cx, r, current, base, last, false) {
        Ok(seq) => seq,
        Err(err) if is_soft(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    combine_pairs(cx, ls.len() * rs.len(), |cx, k| {
        cx.charge(1)?;
        let (a, b) = (&ls[k / rs.len()], &rs[k % rs.len()]);
        let (Some(JsonNode::String(x)), Some(JsonNode::String(y))) = (a.node(), b.node()) else {
            return Ok(None);
        };
        charge_string_compare(cx, x, y)?;
        Ok(Some(x.starts_with(y.as_str())))
    })
}

/// `l like_regex "p"` (§4.4 rule 5): each of `l`'s items (lax-unwrapped) is true when it is a
/// string the pattern matches, false when a string it does not, unknown otherwise. The match
/// charges its `regex_step` units (regex.md §6).
fn eval_like_regex<'d>(
    cx: &mut Ctx<'d, '_>,
    l: &PathExpr,
    re: &LikeRegex,
    current: &Item<'d>,
    base: &Base<'d>,
    last: Option<i64>,
) -> Result<Option<bool>> {
    let Some(ls) = eval_operand(cx, l, current, base, last)? else {
        return Ok(None);
    };
    combine_pairs(cx, ls.len(), |cx, k| {
        cx.charge(1)?;
        let Some(JsonNode::String(subject)) = ls[k].node() else {
            return Ok(None);
        };
        let chars: Vec<char> = if re.flags & REGEX_I != 0 {
            let prop = crate::collation::loaded_property();
            crate::collation::fold_lower_simple(subject, prop.as_deref())
                .chars()
                .collect()
        } else {
            subject.chars().collect()
        };
        Ok(Some(re.program.is_match(&chars, cx.m)?))
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
    last: Option<i64>,
) -> Result<Option<bool>> {
    let Some(ls) = eval_operand(cx, l, current, base, last)? else {
        return Ok(None);
    };
    let Some(rs) = eval_operand(cx, r, current, base, last)? else {
        return Ok(None);
    };
    combine_pairs(cx, ls.len() * rs.len(), |cx, k| {
        cx.charge(1)?;
        let (a, b) = (&ls[k / rs.len()], &rs[k % rs.len()]);
        match (a.node(), b.node()) {
            (Some(JsonNode::String(x)), Some(JsonNode::String(y))) => {
                charge_string_compare(cx, x, y)?;
            }
            (Some(JsonNode::Number(x)), Some(JsonNode::Number(y))) => {
                cx.m.charge(COSTS.decimal_work * (decimal::work_linear(x, y) as i64 - 1));
                cx.m.guard()?;
            }
            _ => {}
        }
        Ok(compare_items(a, op, b))
    })
}

/// Evaluate a comparison operand to its item sequence, an array item unwrapped one level in lax
/// (§4.1 rule 4). `Ok(None)` is a SQL/JSON error, which makes the comparison unknown rather than
/// propagating (§4.2: filter operands never raise). A cost, budget, or cancellation error still
/// propagates (§7).
fn eval_operand<'d>(
    cx: &mut Ctx<'d, '_>,
    e: &PathExpr,
    current: &Item<'d>,
    base: &Base<'d>,
    last: Option<i64>,
) -> Result<Option<Vec<Item<'d>>>> {
    match eval_seq(cx, e, current, base, last, true) {
        Ok(seq) => Ok(Some(seq)),
        Err(err) if is_soft(&err) => Ok(None),
        Err(err) => Err(err),
    }
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
/// object`, where the base is the context item (number 0), a variable's base object (§5), or the
/// `.keyvalue()` object whose continuation is running. PostgreSQL measures the offset in its binary
/// jsonb layout, which is a pure function of the value, so jed computes the same layout
/// arithmetically.
fn keyvalue_id<'d>(cx: &mut Ctx<'d, '_>, item: &Item<'d>, base: &Base<'d>) -> i64 {
    const BASE_STRIDE: i64 = 10_000_000_000;
    match base {
        Base::Root => match item {
            Item::Doc(n) => tree_offset(cx, cx.root, n),
            _ => 0,
        },
        Base::Var { tree, id } => {
            let offset = match item {
                Item::Doc(n) => tree_offset(cx, tree, n),
                _ => 0,
            };
            id * BASE_STRIDE + offset
        }
        Base::Kv(kv) => {
            let offset = match (item, &kv.value) {
                (Item::Kv(k), _) if Rc::ptr_eq(k, kv) => 0,
                // A container inside the pair's value: the value's offset in the pair object plus
                // the container's offset inside the value (both are 4-byte aligned, so the value's
                // internal layout does not depend on where it sits). The value's base object was
                // laid out when the `.keyvalue()` that made the pair numbered it.
                (Item::Doc(n), Item::Doc(v)) => {
                    let value_at = align4(
                        40 + pg_numeric_size(&Decimal::from_i64(kv.id)) + kv.key.len() as i64,
                    );
                    value_at + laid_out_offset(cx, n) - laid_out_offset(cx, v)
                }
                _ => 0,
            };
            kv.generated * BASE_STRIDE + offset
        }
    }
}

/// A container's offset in the binary layout of the base object `tree` that holds it, laying the
/// base object out first if no `.keyvalue()` has yet.
fn tree_offset(cx: &mut Ctx<'_, '_>, tree: &JsonNode, n: &JsonNode) -> i64 {
    let key = tree as *const JsonNode;
    if !cx.laid_out.contains(&key) {
        cx.laid_out.push(key);
        pg_layout(tree, 0, &mut cx.offsets);
    }
    laid_out_offset(cx, n)
}

/// A container's offset in its already laid-out base object.
fn laid_out_offset(cx: &Ctx<'_, '_>, n: &JsonNode) -> i64 {
    cx.offsets
        .get(&(n as *const JsonNode))
        .copied()
        .unwrap_or(0)
}

/// The value of the path variable `$name` with the base object its continuation runs under
/// (PostgreSQL's `getJsonPathVariable`): a member of the `vars` object, found by a binary search of
/// its canonical key order, or the first `PASSING` binding of that name. A name with no binding is
/// `42704`, which neither lax mode, silent mode, nor a comparison operand suppresses.
fn variable<'d>(cx: &Ctx<'d, '_>, name: &str) -> Result<(Item<'d>, Base<'d>)> {
    let found = match cx.vars {
        Vars::None => None,
        Vars::Object(tree) => match tree {
            JsonNode::Object(members) => members
                .binary_search_by(|(k, _)| crate::json::key_cmp(k, name))
                .ok()
                .map(|i| (&members[i].1, Base::Var { tree, id: 1 })),
            _ => None,
        },
        Vars::Passing { vars, .. } => vars
            .binary_search_by(|v| v.name.as_bytes().cmp(name.as_bytes()))
            .ok()
            .map(|i| {
                let v = &vars[i];
                (
                    &v.value,
                    Base::Var {
                        tree: &v.value,
                        id: v.id,
                    },
                )
            }),
    };
    match found {
        Some((node, base)) => Ok((Item::Doc(node), base)),
        None => Err(EngineError::new(
            SqlState::UndefinedObject,
            format!("could not find jsonpath variable \"{name}\""),
        )),
    }
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
        Index::Expr(e) => write_expr(e, false, out),
    }
}

/// Render a member key as a canonical jsonpath quoted string — PostgreSQL's `escape_json`: `\"`,
/// `\\`, `\b \f \n \r \t`, and any other control character as `\u00XX`.
fn write_quoted(k: &str, out: &mut String) {
    out.push('"');
    for c in k.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// PostgreSQL's jsonpath scanner `special` set (jsonpath_scan.l): each is a token of its own, so it
/// ends an unquoted name.
fn is_special(c: u8) -> bool {
    b"?%$.[]{}()|&!=<>@#,*:-+/".contains(&c)
}

/// PostgreSQL's jsonpath `blank` set — whitespace.
fn is_blank(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

/// PostgreSQL's jsonpath `other` class: a byte that may appear unescaped in an unquoted name — not
/// special, blank, `\`, or `"`. Every non-ASCII byte is one.
fn is_other(c: u8) -> bool {
    !is_special(c) && !is_blank(c) && c != b'\\' && c != b'"'
}

/// The length of the longest PostgreSQL jsonpath number token at the start of `s` (which starts with
/// a digit), counting the rules a name competes with: an integer (decimal, or a `0x`/`0o`/`0b`
/// prefix), a decimal, a real, an incomplete exponent (`1e+`), and each with one trailing `other`
/// byte (the "trailing junk" rules). `_` digit separators count, as PostgreSQL 16+ lexes them.
fn pg_number_len(s: &[u8]) -> usize {
    fn dec(c: u8) -> bool {
        c.is_ascii_digit()
    }
    fn hex(c: u8) -> bool {
        c.is_ascii_hexdigit()
    }
    fn oct(c: u8) -> bool {
        (b'0'..=b'7').contains(&c)
    }
    fn bin(c: u8) -> bool {
        c == b'0' || c == b'1'
    }
    // The end of `(_?digit)*` from `j` (a first digit precedes it).
    let digit_run = |mut j: usize, ok: fn(u8) -> bool| -> usize {
        loop {
            match s.get(j) {
                Some(&c) if ok(c) => j += 1,
                Some(b'_') if s.get(j + 1).is_some_and(|&c| ok(c)) => j += 2,
                _ => return j,
            }
        }
    };
    // A "trailing junk" rule: the token plus one `other` byte.
    let junk = |end: usize| -> usize {
        if s.get(end).is_some_and(|&c| is_other(c)) {
            end + 1
        } else {
            end
        }
    };
    // decinteger: `0`, or a nonzero digit and (_?digit)*.
    let int_end = if s.first() == Some(&b'0') {
        1
    } else {
        digit_run(1, dec)
    };
    let mut best = junk(int_end);
    // hexinteger / octinteger / bininteger: `0x`, `0o`, `0b` and one or more digits.
    if s.first() == Some(&b'0') {
        let ok: Option<fn(u8) -> bool> = match s.get(1) {
            Some(b'x' | b'X') => Some(hex),
            Some(b'o' | b'O') => Some(oct),
            Some(b'b' | b'B') => Some(bin),
            _ => None,
        };
        if let Some(ok) = ok.filter(|ok| s.get(2).is_some_and(|&c| ok(c))) {
            best = best.max(digit_run(3, ok));
        }
    }
    // decimal: decinteger `.` decdigits?
    let mut dec_end = None;
    if s.get(int_end) == Some(&b'.') {
        let frac = int_end + 1;
        let end = if s.get(frac).is_some_and(|&c| dec(c)) {
            digit_run(frac + 1, dec)
        } else {
            frac
        };
        best = best.max(junk(end));
        dec_end = Some(end);
    }
    // real: (decinteger | decimal) [Ee] [-+]? decdigits; realfail: … [Ee] [-+].
    for m in std::iter::once(int_end).chain(dec_end) {
        if !matches!(s.get(m), Some(b'e' | b'E')) {
            continue;
        }
        let mut k = m + 1;
        let signed = matches!(s.get(k), Some(b'-' | b'+'));
        if signed {
            k += 1;
        }
        if s.get(k).is_some_and(|&c| dec(c)) {
            best = best.max(junk(digit_run(k + 1, dec)));
        } else if signed {
            best = best.max(k);
        }
    }
    best
}

/// Whether PostgreSQL keyword `kw` matches the decoded word `w`: exactly for `null` / `true` /
/// `false`, ASCII-case-insensitively for the rest (`checkKeyword`).
fn keyword_is(w: &str, kw: &str) -> bool {
    if matches!(kw, "null" | "true" | "false") {
        w == kw
    } else {
        w.eq_ignore_ascii_case(kw)
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    /// The program nesting depth (spec/design/json.md §6.4): +1 per filter `?(`, parenthesized
    /// group, `exists (`, `!`, `&&`/`||` chain step, arithmetic operator, and subscript list,
    /// bounded by [`MAX_JSON_DEPTH`] → `54001`. Bounding it at the compiler bounds every derived
    /// walk (render, evaluate, `Clone`/`Drop`).
    depth: usize,
    /// How many filters enclose the cursor — `@` is valid only inside one.
    in_filter: usize,
    /// How many subscripts enclose the cursor — `last` is valid only inside one.
    in_subscript: usize,
    /// The instruction count of the `like_regex` programs compiled so far.
    regex_insts: i64,
}

/// A parsed operand: an expression or a predicate — PostgreSQL's `expr_or_predicate`. A
/// parenthesized predicate is an expression only when accessors follow it.
enum Parsed {
    Expr(PathExpr),
    Pred(Pred),
    /// A predicate that is exactly `( predicate )`, the only operand of `is unknown`.
    ParenPred(Pred),
}

/// Require an expression where the grammar takes one (`42601` for a bare predicate).
fn need_expr(p: Parsed) -> Result<PathExpr> {
    match p {
        Parsed::Expr(e) => Ok(e),
        Parsed::Pred(_) | Parsed::ParenPred(_) => {
            Err(malformed("expected an expression, not a predicate"))
        }
    }
}

/// Require a predicate where the grammar takes one (`42601` for an expression).
fn need_pred(p: Parsed) -> Result<Pred> {
    match p {
        Parsed::Pred(pred) | Parsed::ParenPred(pred) => Ok(pred),
        Parsed::Expr(_) => Err(malformed("expected a predicate")),
    }
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

    /// Skip what PostgreSQL's jsonpath scanner discards between tokens (jsonpath.md §2.4): whitespace
    /// (`[ \t\n\r\f]`), `/* … */` comments, and a name that runs straight into a comment — the
    /// scanner's non-quoted-string state enters the comment state without returning the name, so
    /// `$.a/*c*/b` is `$."b"`. The name is still lexed, so its escape error is raised.
    fn skip_ws(&mut self) -> Result<()> {
        loop {
            while self.peek().is_some_and(is_blank) {
                self.i += 1;
            }
            if self.s[self.i..].starts_with(b"/*") {
                self.skip_comment()?;
                continue;
            }
            if self.word_ahead() {
                let start = self.i;
                self.scan_word()?;
                if self.s[self.i..].starts_with(b"/*") {
                    self.skip_comment()?;
                    continue;
                }
                self.i = start;
            }
            return Ok(());
        }
    }

    /// Skip the `/* … */` comment at the cursor. Comments do not nest; one without its `*/` is
    /// `42601`.
    fn skip_comment(&mut self) -> Result<()> {
        let body = self.i + 2;
        match self.s[body..].windows(2).position(|w| w == b"*/") {
            Some(k) => {
                self.i = body + k + 2;
                Ok(())
            }
            None => Err(malformed("unexpected end of comment")),
        }
    }

    /// Whether PostgreSQL's scanner would lex a name (an identifier or keyword) at the cursor: a
    /// backslash escape or an `other` byte. A digit starts a number instead, unless the run of
    /// `other` bytes there is longer than every number token (flex's longest match: `1abc` is a
    /// name, `12`, `1a`, and `1e3` are not).
    fn word_ahead(&self) -> bool {
        match self.peek() {
            Some(b'\\') => true,
            Some(c) if c.is_ascii_digit() => {
                let rest = &self.s[self.i..];
                let run = rest.iter().take_while(|&&c| is_other(c)).count();
                run > pg_number_len(rest)
            }
            Some(c) => is_other(c),
            None => false,
        }
    }

    /// Scan the name at the cursor ([`Self::word_ahead`]) — PostgreSQL's non-quoted string state: a
    /// run of `other` bytes and backslash escapes (decoded as in a quoted string, jsonpath.md §2.1),
    /// ended by a special byte, whitespace, `"`, or the end of input.
    fn scan_word(&mut self) -> Result<String> {
        let mut out = String::new();
        loop {
            match self.peek() {
                Some(b'\\') => self.parse_escape(&mut out)?,
                Some(c) if is_other(c) => self.push_char(&mut out),
                _ => return Ok(out),
            }
        }
    }

    /// Consume keyword `kw` if the name at the cursor is that keyword ([`keyword_is`]). A name that
    /// is not — or whose escapes fail to decode — is left unconsumed.
    fn eat_keyword(&mut self, kw: &str) -> bool {
        if !self.word_ahead() {
            return false;
        }
        let start = self.i;
        match self.scan_word() {
            Ok(w) if keyword_is(&w, kw) => true,
            _ => {
                self.i = start;
                false
            }
        }
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
                Some(b'\\') => self.parse_escape(&mut out)?,
                Some(_) => self.push_char(&mut out),
            }
        }
    }

    /// Decode the backslash escape at the cursor into `out` — shared by quoted strings and unquoted
    /// names (jsonpath.md §2.1).
    fn parse_escape(&mut self, out: &mut String) -> Result<()> {
        match self.s.get(self.i + 1).copied() {
            None => return Err(malformed("unexpected end after backslash")),
            Some(b'u') => self.parse_unicode_run(out)?,
            Some(b'x') => {
                let hex = self.s.get(self.i + 2..self.i + 4);
                let ch = hex
                    .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .ok_or_else(|| malformed("invalid hexadecimal character sequence"))?;
                push_code_point(ch, out)?;
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
                        self.push_char(out);
                        return Ok(());
                    }
                }
                self.i += 1;
            }
        }
        Ok(())
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
    /// is `index` or `index to index`, an index any expression (`1`, `last - 1`, `$.i`), in which
    /// `last` is allowed.
    fn parse_subscripts(&mut self) -> Result<Vec<Subscript>> {
        let mut subs = Vec::new();
        self.deepen()?;
        self.in_subscript += 1;
        loop {
            let a = self.parse_index()?;
            self.skip_ws()?;
            let sub = if self.eat_keyword("to") {
                let b = self.parse_index()?;
                self.skip_ws()?;
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
                    self.in_subscript -= 1;
                    self.depth -= 1;
                    return Ok(subs);
                }
                _ => return Err(malformed("expected `,` or `]` in subscript")),
            }
        }
    }

    /// One subscript index: a number literal or `last` keeps its own form; anything else is an
    /// expression evaluated per array.
    fn parse_index(&mut self) -> Result<Index> {
        self.skip_ws()?;
        if self.peek().is_none() {
            return Err(malformed("expected a subscript index"));
        }
        let e = need_expr(self.parse_additive()?)?;
        Ok(match e {
            PathExpr {
                primary: Primary::Lit(JsonNode::Number(n)),
                steps,
            } if steps.is_empty() => Index::Number(n),
            PathExpr {
                primary: Primary::Last,
                steps,
            } if steps.is_empty() => Index::Last,
            e => Index::Expr(Box::new(e)),
        })
    }

    /// Parse a primary and its accessor steps (PostgreSQL's `accessor_expr`). A parenthesized
    /// expression followed by accessors continues it (`($.a).b` is `$.a.b`, `(1 + 2).type()`); a
    /// parenthesized predicate followed by accessors yields its truth value; without accessors the
    /// parentheses only group.
    fn parse_postfix(&mut self) -> Result<Parsed> {
        self.skip_ws()?;
        if self.eat(b'(') {
            self.deepen()?;
            let inner = self.parse_or()?;
            self.depth -= 1;
            self.skip_ws()?;
            if !self.eat(b')') {
                return Err(malformed("expected `)`"));
            }
            let steps = self.parse_steps()?;
            if steps.is_empty() {
                return Ok(match inner {
                    Parsed::Pred(pred) | Parsed::ParenPred(pred) => Parsed::ParenPred(pred),
                    expr => expr,
                });
            }
            return Ok(Parsed::Expr(match inner {
                Parsed::Expr(mut e) => {
                    e.steps.extend(steps);
                    e
                }
                Parsed::Pred(pred) | Parsed::ParenPred(pred) => PathExpr {
                    primary: Primary::Pred(Box::new(pred)),
                    steps,
                },
            }));
        }
        let primary = match self.peek() {
            Some(b'$') => {
                self.i += 1;
                // `$"name"` / `$name` — a path variable. PostgreSQL's unquoted variable name is a run
                // of `other` bytes (no escapes); a quoted one takes the string escapes (§2.1).
                match self.peek() {
                    Some(b'"') => Primary::Var(self.parse_quoted()?),
                    Some(c) if is_other(c) => {
                        let start = self.i;
                        while self.peek().is_some_and(is_other) {
                            self.i += 1;
                        }
                        // A run of `other` bytes never splits a UTF-8 sequence (every non-ASCII
                        // byte is one), so the slice is valid UTF-8.
                        Primary::Var(
                            String::from_utf8(self.s[start..self.i].to_vec())
                                .expect("a run of other bytes is whole UTF-8 characters"),
                        )
                    }
                    _ => Primary::Root,
                }
            }
            Some(b'@') => {
                if self.in_filter == 0 {
                    return Err(malformed("@ is not allowed in root expressions"));
                }
                self.i += 1;
                Primary::Current
            }
            Some(b'"') => Primary::Lit(JsonNode::String(self.parse_quoted()?)),
            // A digit-led name (`1abc`) is not an operand.
            Some(c) if c.is_ascii_digit() && self.word_ahead() => {
                self.scan_word()?;
                return Err(malformed("a name is not an operand"));
            }
            Some(c) if c.is_ascii_digit() => Primary::Lit(self.parse_number()?),
            Some(b'.') if self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) => {
                Primary::Lit(self.parse_number()?)
            }
            None => return Err(malformed("unexpected end of input")),
            // A name: one of the keywords that start an operand, or else not an operand at all.
            _ if self.word_ahead() => {
                let word = self.scan_word()?;
                if keyword_is(&word, "true") {
                    Primary::Lit(JsonNode::Bool(true))
                } else if keyword_is(&word, "false") {
                    Primary::Lit(JsonNode::Bool(false))
                } else if keyword_is(&word, "null") {
                    Primary::Lit(JsonNode::Null)
                } else if keyword_is(&word, "last") {
                    if self.in_subscript == 0 {
                        return Err(malformed("LAST is allowed only in array subscripts"));
                    }
                    Primary::Last
                } else if keyword_is(&word, "exists") {
                    // No accessors follow `exists (…)`: it is a predicate, not an expression.
                    return Ok(Parsed::Pred(self.parse_exists()?));
                } else {
                    return Err(malformed("a name is not an operand"));
                }
            }
            _ => return Err(malformed("unexpected token")),
        };
        Ok(Parsed::Expr(PathExpr {
            primary,
            steps: self.parse_steps()?,
        }))
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
                // `1.e3` — an empty fraction, then an exponent.
                Some(b'e' | b'E')
                    if !int_part.is_empty()
                        && (self.peek_at(2).is_some_and(|c| c.is_ascii_digit())
                            || (matches!(self.peek_at(2), Some(b'+' | b'-'))
                                && self.peek_at(3).is_some_and(|c| c.is_ascii_digit()))) =>
                {
                    self.i += 1;
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
        // PostgreSQL's "trailing junk" rules: a number followed by any `other` byte (`1a`, `1~`).
        if self.peek().is_some_and(is_other) {
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
            self.skip_ws()?;
            match self.peek() {
                Some(b'.') => {
                    self.i += 1;
                    // PostgreSQL lexes `.5` / `.1abc` as a number token, never `.` and a name.
                    if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                        return Err(malformed("expected a member name after `.`"));
                    }
                    self.skip_ws()?;
                    if self.eat(b'*') {
                        if self.eat(b'*') {
                            steps.push(self.parse_any_levels()?);
                        } else {
                            steps.push(Step::WildcardMember);
                        }
                    } else if self.peek() == Some(b'"') {
                        steps.push(Step::Member(self.parse_quoted()?));
                    } else if self.word_ahead() {
                        // An unquoted name (jsonpath.md §2.1); followed by `(`, an item method.
                        let name = self.scan_word()?;
                        let save = self.i;
                        self.skip_ws()?;
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
                    self.skip_ws()?;
                    if self.eat(b'*') {
                        self.skip_ws()?;
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
                    self.skip_ws()?;
                    if !self.eat(b'(') {
                        return Err(malformed("expected `(` after `?`"));
                    }
                    self.deepen()?;
                    self.in_filter += 1;
                    let pred = need_pred(self.parse_or()?)?;
                    self.in_filter -= 1;
                    self.depth -= 1;
                    self.skip_ws()?;
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

    /// Parse the optional `{level}` / `{level to level}` after `.**` (jsonpath.md §2).
    fn parse_any_levels(&mut self) -> Result<Step> {
        let save = self.i;
        self.skip_ws()?;
        if !self.eat(b'{') {
            self.i = save;
            return Ok(Step::Any {
                first: 0,
                last: LEVEL_LAST,
            });
        }
        let first = self.parse_any_level()?;
        let mut last = first;
        self.skip_ws()?;
        if self.eat_keyword("to") {
            last = self.parse_any_level()?;
            self.skip_ws()?;
        }
        if !self.eat(b'}') {
            return Err(malformed("expected `}` after a .** level"));
        }
        Ok(Step::Any { first, last })
    }

    /// Parse one `.**` level: an unsigned integer literal (PostgreSQL's `INT_P`, at most
    /// 2147483647, else `22003`) or `last`.
    fn parse_any_level(&mut self) -> Result<u32> {
        self.skip_ws()?;
        if self.eat_keyword("last") {
            return Ok(LEVEL_LAST);
        }
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let digits = std::str::from_utf8(&self.s[start..self.i]).expect("ASCII digits");
        if digits.is_empty() {
            return Err(malformed("expected a .** level"));
        }
        // A number running into a name character, a leading zero, or a fraction is not an integer.
        if self.peek().is_some_and(|c| c == b'.' || is_other(c))
            || (digits.len() > 1 && digits.starts_with('0'))
        {
            return Err(malformed("trailing junk after numeric literal"));
        }
        match digits.parse::<u32>() {
            Ok(v) if v <= i32::MAX as u32 => Ok(v),
            _ => Err(EngineError::new(
                SqlState::NumericValueOutOfRange,
                format!("value \"{digits}\" is out of range for type integer"),
            )),
        }
    }

    /// Parse an item method's argument list (the cursor is at its `(`). Method names are matched
    /// case-insensitively, like PostgreSQL's keywords.
    fn parse_method(&mut self, name: &str) -> Result<Method> {
        self.i += 1; // (
        self.skip_ws()?;
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
                        self.skip_ws()?;
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
        self.skip_ws()?;
        let neg = if self.eat(b'-') {
            true
        } else {
            self.eat(b'+');
            false
        };
        self.skip_ws()?;
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let digits = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("");
        if digits.is_empty()
            || (digits.len() > 1 && digits.starts_with('0'))
            || self.peek().is_some_and(|c| is_other(c) || c == b'.')
        {
            return Err(malformed("expected an integer .decimal() argument"));
        }
        Ok(Decimal::from_digits_scale(neg, digits, 0))
    }

    /// Parse an expression or predicate: `||` over `&&` over `!` / comparison over arithmetic
    /// (PostgreSQL's `jsonpath_gram.y` precedence: `||`, `&&`, `!`, comparisons, `+ -`, `* / %`,
    /// unary `+ -`, accessors). `||` and `&&` take predicates.
    fn parse_or(&mut self) -> Result<Parsed> {
        let mut left = self.parse_and()?;
        // Each `||` step deepens the left-deep tree by one level (json.md §6.4).
        let mut steps = 0;
        loop {
            self.skip_ws()?;
            if !self.eat_op(b"||") {
                self.depth -= steps;
                return Ok(left);
            }
            self.deepen()?;
            steps += 1;
            let l = need_pred(left)?;
            let r = need_pred(self.parse_and()?)?;
            left = Parsed::Pred(Pred::Or(Box::new(l), Box::new(r)));
        }
    }

    fn parse_and(&mut self) -> Result<Parsed> {
        let mut left = self.parse_not()?;
        // Each `&&` step deepens the left-deep tree by one level (json.md §6.4).
        let mut steps = 0;
        loop {
            self.skip_ws()?;
            if !self.eat_op(b"&&") {
                self.depth -= steps;
                return Ok(left);
            }
            self.deepen()?;
            steps += 1;
            let l = need_pred(left)?;
            let r = need_pred(self.parse_not()?)?;
            left = Parsed::Pred(Pred::And(Box::new(l), Box::new(r)));
        }
    }

    /// `!(predicate)` — the operand must be a parenthesized predicate.
    fn parse_not(&mut self) -> Result<Parsed> {
        self.skip_ws()?;
        if self.peek() == Some(b'!') && self.peek_at(1) != Some(b'=') {
            self.i += 1;
            self.skip_ws()?;
            if self.eat_keyword("exists") {
                let inner = self.parse_exists()?;
                return Ok(Parsed::Pred(Pred::Not(Box::new(inner))));
            }
            if !self.eat(b'(') {
                return Err(malformed("expected `(` after `!`"));
            }
            self.deepen()?;
            let inner = need_pred(self.parse_or()?)?;
            self.depth -= 1;
            self.skip_ws()?;
            if !self.eat(b')') {
                return Err(malformed("expected `)` after `!(`"));
            }
            return Ok(Parsed::Pred(Pred::Not(Box::new(inner))));
        }
        self.parse_comparison()
    }

    /// Parse `exists (expr)` after its keyword (jsonpath.md §2): the operand is an expression, not
    /// a predicate.
    fn parse_exists(&mut self) -> Result<Pred> {
        self.skip_ws()?;
        if !self.eat(b'(') {
            return Err(malformed("expected `(` after exists"));
        }
        self.deepen()?;
        let e = need_expr(self.parse_or()?)?;
        self.depth -= 1;
        self.skip_ws()?;
        if !self.eat(b')') {
            return Err(malformed("expected `)` after exists ("));
        }
        Ok(Pred::Exists(e))
    }

    /// `expr cmp expr`, `expr starts with s`, `expr like_regex "p" [flag "f"]`, `(predicate) is
    /// unknown`, or a lone expression or predicate.
    fn parse_comparison(&mut self) -> Result<Parsed> {
        let left = match self.parse_additive()? {
            Parsed::Pred(pred) => return Ok(Parsed::Pred(pred)),
            // `is unknown` applies only to a parenthesized predicate. Its result is not one, so
            // `(p) is unknown is unknown` is not PostgreSQL's grammar; only a parenthesized one
            // nests.
            Parsed::ParenPred(pred) => {
                self.skip_ws()?;
                if !self.word_ahead() {
                    return Ok(Parsed::Pred(pred));
                }
                let start = self.i;
                if !keyword_is(&self.scan_word()?, "is") {
                    self.i = start;
                    return Ok(Parsed::Pred(pred));
                }
                self.skip_ws()?;
                if !self.eat_keyword("unknown") {
                    return Err(malformed("expected `unknown` after `is`"));
                }
                return Ok(Parsed::Pred(Pred::IsUnknown(Box::new(pred))));
            }
            Parsed::Expr(e) => e,
        };
        self.skip_ws()?;
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
            // A word predicate operator. The name is lexed first, so its escape error wins; any
            // other name is trailing input.
            if self.word_ahead() {
                let start = self.i;
                let word = self.scan_word()?;
                if keyword_is(&word, "starts") {
                    return self.parse_starts_with(left);
                }
                if keyword_is(&word, "like_regex") {
                    return self.parse_like_regex(left);
                }
                self.i = start;
            }
            return Ok(Parsed::Expr(left));
        };
        let right = need_expr(self.parse_additive()?)?;
        Ok(Parsed::Pred(Pred::Compare(left, op, right)))
    }

    /// Parse the rest of `expr starts with s` after `starts`: `s` is a string literal or a variable
    /// (jsonpath.md §2).
    fn parse_starts_with(&mut self, left: PathExpr) -> Result<Parsed> {
        self.skip_ws()?;
        if !self.eat_keyword("with") {
            return Err(malformed("expected `with` after `starts`"));
        }
        self.skip_ws()?;
        let right = match self.peek() {
            Some(b'"') => PathExpr {
                primary: Primary::Lit(JsonNode::String(self.parse_quoted()?)),
                steps: Vec::new(),
            },
            Some(b'$') => match self.parse_postfix()? {
                Parsed::Expr(
                    e @ PathExpr {
                        primary: Primary::Var(_),
                        ..
                    },
                ) if e.steps.is_empty() => e,
                _ => return Err(malformed("starts with takes a string or a variable")),
            },
            _ => return Err(malformed("starts with takes a string or a variable")),
        };
        Ok(Parsed::Pred(Pred::StartsWith(left, right)))
    }

    /// Parse the rest of `expr like_regex "pattern" [flag "flags"]` after `like_regex` and compile
    /// the pattern (jsonpath.md §4.3): an unknown flag is `42601`, `x` or `m` without `q` `0A000`,
    /// a malformed pattern `2201B`.
    fn parse_like_regex(&mut self, left: PathExpr) -> Result<Parsed> {
        self.skip_ws()?;
        if self.peek() != Some(b'"') {
            return Err(malformed("like_regex takes a string pattern"));
        }
        let pattern = self.parse_quoted()?;
        let mut flags: u8 = 0;
        let save = self.i;
        self.skip_ws()?;
        if self.eat_keyword("flag") {
            self.skip_ws()?;
            if self.peek() != Some(b'"') {
                return Err(malformed("flag takes a string"));
            }
            for c in self.parse_quoted()?.chars() {
                let Some(i) = REGEX_FLAG_CHARS.iter().position(|&f| f as char == c) else {
                    return Err(malformed(&format!(
                        "unrecognized flag character \"{c}\" in LIKE_REGEX predicate"
                    )));
                };
                flags |= 1 << i;
            }
        } else {
            self.i = save;
        }
        if flags & REGEX_Q == 0 {
            if flags & REGEX_X != 0 {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "XQuery \"x\" flag (expanded regular expressions) is not implemented",
                ));
            }
            if flags & REGEX_M != 0 {
                return Err(unsupported("like_regex \"m\" flag"));
            }
        }
        let program = compile_like_regex(&pattern, flags)?;
        self.regex_insts += program.ninst() as i64;
        Ok(Parsed::Pred(Pred::LikeRegex(
            left,
            Box::new(LikeRegex {
                pattern,
                flags,
                program: Rc::new(program),
            }),
        )))
    }

    /// `+` and `-`, left-associative.
    fn parse_additive(&mut self) -> Result<Parsed> {
        let mut left = self.parse_multiplicative()?;
        // Each operator deepens the left-deep tree by one level (json.md §6.4).
        let mut steps = 0;
        loop {
            self.skip_ws()?;
            let op = match self.peek() {
                Some(b'+') => ArithOp::Add,
                Some(b'-') => ArithOp::Sub,
                _ => {
                    self.depth -= steps;
                    return Ok(left);
                }
            };
            self.i += 1;
            self.deepen()?;
            steps += 1;
            let l = need_expr(left)?;
            let r = need_expr(self.parse_multiplicative()?)?;
            left = Parsed::Expr(binary(op, l, r));
        }
    }

    /// `*`, `/`, and `%`, left-associative.
    fn parse_multiplicative(&mut self) -> Result<Parsed> {
        let mut left = self.parse_unary()?;
        let mut steps = 0;
        loop {
            self.skip_ws()?;
            let op = match self.peek() {
                Some(b'*') => ArithOp::Mul,
                Some(b'/') => ArithOp::Div,
                Some(b'%') => ArithOp::Mod,
                _ => {
                    self.depth -= steps;
                    return Ok(left);
                }
            };
            self.i += 1;
            self.deepen()?;
            steps += 1;
            let l = need_expr(left)?;
            let r = need_expr(self.parse_unary()?)?;
            left = Parsed::Expr(binary(op, l, r));
        }
    }

    /// Unary `+` / `-`, which bind tighter than the binary operators but looser than accessors
    /// (`-$.a * 2` is `(-($.a)) * 2`). Applied to a number literal they fold into it (`-1`), as
    /// PostgreSQL's grammar does.
    fn parse_unary(&mut self) -> Result<Parsed> {
        self.skip_ws()?;
        let op = match self.peek() {
            Some(b'+') => UnaryOp::Plus,
            Some(b'-') => UnaryOp::Minus,
            _ => return self.parse_postfix(),
        };
        self.i += 1;
        self.deepen()?;
        let arg = need_expr(self.parse_unary()?)?;
        self.depth -= 1;
        Ok(Parsed::Expr(match (op, arg) {
            (
                UnaryOp::Plus,
                arg @ PathExpr {
                    primary: Primary::Lit(JsonNode::Number(_)),
                    ..
                },
            ) if arg.steps.is_empty() => arg,
            (
                UnaryOp::Minus,
                PathExpr {
                    primary: Primary::Lit(JsonNode::Number(n)),
                    steps,
                },
            ) if steps.is_empty() => PathExpr {
                primary: Primary::Lit(JsonNode::Number(n.neg())),
                steps,
            },
            (op, arg) => PathExpr {
                primary: Primary::Unary(op, Box::new(arg)),
                steps: Vec::new(),
            },
        }))
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

/// Compile a `like_regex` pattern under its flags (jsonpath.md §4.3, regex.md §8d): `i` folds the
/// pattern, `q` quotes it, `s` is dot-all, and otherwise the mode is newline-sensitive. A quoted
/// pattern is all literal, so `i` folds every character of it; otherwise the fold keeps the letter
/// after a backslash (regex.md §1).
fn compile_like_regex(pattern: &str, flags: u8) -> Result<crate::regex::Program> {
    let folded;
    let pattern = if flags & REGEX_I != 0 {
        let prop = crate::collation::loaded_property();
        folded = if flags & REGEX_Q != 0 {
            crate::collation::fold_lower_simple(pattern, prop.as_deref())
        } else {
            crate::regex::fold_pattern(pattern, prop.as_deref())
        };
        folded.as_str()
    } else {
        pattern
    };
    if flags & REGEX_Q != 0 {
        return crate::regex::compile_mode(
            &crate::regex::quote_regex(pattern),
            crate::regex::Mode::default(),
        );
    }
    let mode = if flags & REGEX_S != 0 {
        crate::regex::Mode {
            dot_all: true,
            nl_stop: false,
        }
    } else {
        crate::regex::Mode {
            dot_all: false,
            nl_stop: true,
        }
    };
    crate::regex::compile_mode(pattern, mode)
}

/// A binary arithmetic expression.
fn binary(op: ArithOp, l: PathExpr, r: PathExpr) -> PathExpr {
    PathExpr {
        primary: Primary::Binary(op, Box::new(l), Box::new(r)),
        steps: Vec::new(),
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
