//! The resolved expression IR: `ResolvedType`, the operator and function identifiers, and
//! `RExpr` itself. Parse gives `Expr` (names); resolve gives `RExpr` (column indices, known
//! result types, folded constants); eval walks it per row.

use super::*;

// ============================================================================
// Resolved expression layer.
//
// Parse → `Expr` (names) → resolve → `RExpr` (column indices, known result types,
// folded constants) → eval per row → `Value`. The resolver is where all
// type-checking and the literal range-check live; the evaluator is a pure tree-walk.
// ============================================================================

/// The static type of a resolved expression. `Null` is an untyped NULL literal (its
/// type, if needed, is settled by the surrounding operator/context). `Text` is the
/// `text` family (one collation, `C`); it does not promote.
///
/// Not `Copy`: the `Composite` arm owns a heap shape (open types — CLAUDE.md §4), so the type
/// is cloned/borrowed rather than copied. Every other arm is still a trivial tag/scalar.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ResolvedType {
    Int(ScalarType),
    Bool,
    Text,
    /// The decimal family (one type; the per-column typmod is carried separately, not here).
    Decimal,
    /// The bytea family (raw bytes); does not promote.
    Bytea,
    /// The uuid family (fixed 16 bytes); does not promote. The first non-integer key type.
    Uuid,
    /// The `timestamp` family (zoneless instant). Does not compare/cast to `Timestamptz`.
    Timestamp,
    /// The `timestamptz` family (UTC instant). Does not compare/cast to `Timestamp`.
    Timestamptz,
    /// The `interval` family (a span). Compares only with itself (by the canonical span).
    Interval,
    /// The `date` family (calendar date). A strict island — compares only with itself (by the
    /// i32 day count); no implicit cast to `timestamp` this slice (spec/design/date.md §4).
    Date,
    /// The float family, carrying its width (spec/design/float.md §2). The two widths form a
    /// promotion tower: `f32 → f64` is the one implicit float cast; mixed-width arithmetic
    /// and comparison promote to `f64` first. A strict island — no implicit int/decimal ↔ float.
    Float(ScalarType),
    Null,
    /// A composite (row) type (spec/design/composite.md §5). `name` is `Some` for a named catalog
    /// type — rendered in the `# types:` output and the basis for cross-comparability — or `None`
    /// for an anonymous `ROW(...)` result. `fields` are the resolved (field-name, type) pairs in
    /// declaration order (the basis for field access — S4 — and structural assignability). Boxed so
    /// the common scalar `ResolvedType` stays small.
    Composite(Box<CompositeRType>),
    /// An array type (spec/design/array.md §2), carrying its resolved element type. Two arrays are
    /// comparable iff their element types are equal; an array is assignable to an array column of
    /// the same element type. Boxed to keep the scalar `ResolvedType` small.
    Array(Box<ResolvedType>),
    /// A range type (spec/design/ranges.md §2), carrying its resolved element (subtype) type. Two
    /// ranges are comparable iff their elements are equal; a range is assignable to a range column
    /// of the same element. The element is always one of the six scalar subtypes. Boxed to keep the
    /// scalar `ResolvedType` small.
    Range(Box<ResolvedType>),
    /// The `json` family (verbatim text — spec/design/json.md §4). NOT comparable (PG ships no
    /// btree/hash opclass — §5): a comparison/ORDER BY/DISTINCT on json resolves to 42883.
    Json,
    /// The `jsonb` family (canonical binary — spec/design/json.md §2). Comparable with itself by
    /// PG's total btree order (§5).
    Jsonb,
    /// The `jsonpath` type (spec/design/jsonpath.md, P1a). NOT comparable (42883); literal-only.
    JsonPath,
}

/// The resolved shape of a composite type — its (optional) name and resolved field list. The
/// `name` is `None` for an anonymous `ROW(...)` result, `Some` for a named catalog type.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CompositeRType {
    pub(crate) name: Option<String>,
    pub(crate) fields: Vec<(String, ResolvedType)>,
}

impl ResolvedType {
    /// Whether a `timestamptz` occurs anywhere in this type — itself, an array element, a composite
    /// field, or a range subtype, at any depth. Rendering such a value to text reads the session zone
    /// (timezones.md §9.5).
    pub(crate) fn contains_timestamptz(&self) -> bool {
        match self {
            ResolvedType::Timestamptz => true,
            ResolvedType::Array(e) | ResolvedType::Range(e) => e.contains_timestamptz(),
            ResolvedType::Composite(c) => c.fields.iter().any(|(_, t)| t.contains_timestamptz()),
            _ => false,
        }
    }

    /// Whether this type carries a collation (spec/design/collation.md §1): text or text[].
    pub(crate) fn collatable(&self) -> bool {
        match self {
            ResolvedType::Text => true,
            ResolvedType::Array(e) => matches!(**e, ResolvedType::Text),
            _ => false,
        }
    }

    /// This type's name, for the `# types:` output and a `42804` assignability message (the integer
    /// width is exact). A named composite is its type name; an anonymous `ROW(...)` is `record` (PG).
    pub(crate) fn type_name(&self) -> String {
        match self {
            ResolvedType::Int(st) => st.canonical_name().to_string(),
            ResolvedType::Bool => "boolean".to_string(),
            ResolvedType::Text => "text".to_string(),
            ResolvedType::Decimal => "decimal".to_string(),
            ResolvedType::Bytea => "bytea".to_string(),
            ResolvedType::Uuid => "uuid".to_string(),
            ResolvedType::Timestamp => "timestamp".to_string(),
            ResolvedType::Timestamptz => "timestamptz".to_string(),
            ResolvedType::Interval => "interval".to_string(),
            ResolvedType::Date => "date".to_string(),
            ResolvedType::Float(st) => st.canonical_name().to_string(),
            ResolvedType::Null => "unknown".to_string(),
            ResolvedType::Composite(c) => c.name.clone().unwrap_or_else(|| "record".to_string()),
            ResolvedType::Array(elem) => format!("{}[]", elem.type_name()),
            ResolvedType::Json => "json".to_string(),
            ResolvedType::JsonPath => "jsonpath".to_string(),
            ResolvedType::Jsonb => "jsonb".to_string(),
            // A range names itself by its element subtype (i32 → i32range — spec/design/ranges.md).
            ResolvedType::Range(elem) => resolved_range_element_scalar(elem)
                .and_then(crate::range::range_name_for_element)
                .map(|n| n.to_string())
                .unwrap_or_else(|| format!("range<{}>", elem.type_name())),
        }
    }

    /// Whether a projected value of this type is assignable to a `col_ty` column for storage —
    /// the FAMILY-level gate `INSERT ... SELECT` applies up front (spec/design/grammar.md §24),
    /// before any row is produced (so it fires even over an empty source). It is the
    /// authoritative family-level subset of `store_value` and MUST agree with it: an integer
    /// assigns to an integer or decimal column (int→decimal widens), a decimal only to a decimal column
    /// (decimal→int is explicit-CAST only), text only to text (a text-typed expression never
    /// becomes a uuid/bytea/datetime value without a CAST, as in PostgreSQL; a bare string literal
    /// adapts before this gate — at resolve, or via [`string_literal_adapts_to`] for an
    /// `INSERT ... SELECT` projection), boolean→boolean, uuid→uuid, bytea→bytea, JSON identities plus the
    /// assignment-only jsonb→json rendering cast, a timestamp only to a timestamp
    /// column and a timestamptz only to a timestamptz column (the two never cross — they do not
    /// even compare, timestamp.md), and a NULL-typed projection to any column (a NOT NULL target
    /// then traps 23502 per row). A non-assignable pair is a 42804.
    pub(crate) fn assignable_to(&self, col_ty: ScalarType) -> bool {
        match self {
            ResolvedType::Null => true,
            // A composite source never assigns to a scalar column (the composite-target case is
            // handled structurally at the call site — spec/design/composite.md §4).
            ResolvedType::Composite(_) => false,
            // An array source never assigns to a scalar column (INSERT ... SELECT into an array
            // column is deferred — spec/design/array.md §12).
            ResolvedType::Array(_) => false,
            // A range source never assigns to a scalar column (a range column is not yet storable —
            // spec/design/ranges.md §8; range storage lands in R2).
            ResolvedType::Range(_) => false,
            ResolvedType::Int(_) => col_ty.is_integer() || col_ty.is_decimal(),
            ResolvedType::Decimal => col_ty.is_decimal(),
            ResolvedType::Bool => col_ty.is_bool(),
            ResolvedType::Text => col_ty.is_text(),
            ResolvedType::Bytea => col_ty.is_bytea(),
            ResolvedType::Uuid => col_ty.is_uuid(),
            ResolvedType::Json => col_ty.is_json(),
            ResolvedType::Jsonb => col_ty.is_jsonb() || col_ty.is_json(),
            // jsonpath columns remain unsupported (literal-only), so it is not storage-assignable.
            ResolvedType::JsonPath => false,
            ResolvedType::Timestamp => col_ty.is_timestamp(),
            ResolvedType::Timestamptz => col_ty.is_timestamptz(),
            ResolvedType::Interval => col_ty.is_interval(),
            ResolvedType::Date => col_ty.is_date(),
            // A float assigns to a float column of equal-or-wider width: f32 → f32/f64
            // (the implicit widening cast), f64 → f64 only (f64 → f32 is explicit).
            // store_value enforces the same rule per row (spec/types/casts.toml).
            ResolvedType::Float(st) => col_ty.is_float() && st.rank() <= col_ty.rank(),
        }
    }
}

/// Whether a bare string literal projected by an `INSERT ... SELECT` source adapts to a `col_ty`
/// column: the string-native types, whose input the per-row store parses (22P02/22007/22008 on
/// malformed input). PostgreSQL leaves such a literal `unknown` until it meets the target column
/// (spec/design/grammar.md §24); a numeric or boolean target still rejects it (42804).
pub(crate) fn string_literal_adapts_to(col_ty: ScalarType) -> bool {
    col_ty.is_uuid()
        || col_ty.is_bytea()
        || col_ty.is_timestamp()
        || col_ty.is_timestamptz()
        || col_ty.is_interval()
        || col_ty.is_date()
}

/// Render a projection's resolved types as their canonical names for the public `Outcome::Query`
/// — the `# types:` directive's assertion surface (spec/design/conformance.md §7). Same names as
/// the `42804` message (`type_name`): the exact integer width, the unconstrained `decimal`.
pub(crate) fn type_names(types: &[ResolvedType]) -> Vec<String> {
    types.iter().map(|t| t.type_name().to_string()).collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl ArithOp {
    /// The catalog operator name (catalog.toml) for this arithmetic op — the key for its
    /// per-operator `cost` base (functions.md §8, `operator_cost`).
    pub(crate) fn op_name(self) -> &'static str {
        match self {
            ArithOp::Add => "add",
            ArithOp::Sub => "sub",
            ArithOp::Mul => "mul",
            ArithOp::Div => "div",
            ArithOp::Mod => "mod",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CmpOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

impl CmpOp {
    /// The catalog operator name (catalog.toml) for this comparison — the key for its
    /// per-operator `cost` base (functions.md §8, `operator_cost`).
    pub(crate) fn op_name(self) -> &'static str {
        match self {
            CmpOp::Eq => "eq",
            CmpOp::Ne => "ne",
            CmpOp::Lt => "lt",
            CmpOp::Gt => "gt",
            CmpOp::Le => "le",
            CmpOp::Ge => "ge",
        }
    }
}

/// The scalar functions (kind = "function", spec/design/functions.md §9), parsed from a call
/// name (exact name). Evaluated per row; the overload (integer vs decimal) is recovered
/// at eval from the argument's runtime value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScalarFunc {
    Abs,
    Round,
    // Float functions (spec/design/float.md §8). EXACT / correctly-rounded (in-contract):
    Ceil,
    Floor,
    Trunc,
    Sqrt,
    // TRANSCENDENTAL (exempted — native libm, may differ by an ULP cross-core):
    Exp,
    Ln,
    Log10,
    Pow,
    /// log — base-10 (1-arg) / arbitrary-base (2-arg) logarithm over decimal (decimal.md §8).
    /// Decimal-only (no float `log` overload); the EXACT-numeric kernel, IN-CONTRACT.
    Log,
    Sin,
    Cos,
    Tan,
    /// cbrt — the real cube root (float.md §8). Transcendental/irrational, exempted; no domain
    /// restriction (cbrt of a negative is the negative real root).
    Cbrt,
    /// pi() → f64 — the constant π (float.md §8). Zero-arg; IN-CONTRACT (the same f64 literal in
    /// every core), so NOT in the transcendental ledger.
    Pi,
    /// radians(x) → f64 — degrees → radians (float.md §8): x · RADIANS_PER_DEGREE. A single
    /// correctly-rounded IEEE multiply, IN-CONTRACT (not ledgered).
    Radians,
    /// degrees(x) → f64 — radians → degrees (float.md §8): x / RADIANS_PER_DEGREE. A single
    /// correctly-rounded IEEE divide, IN-CONTRACT (not ledgered).
    Degrees,
    /// asin(x) → f64 — inverse sine in radians (float.md §8). Transcendental, exempted; domain
    /// [-1, 1], |x| > 1 (or ±Inf) → 22003, NaN propagates.
    Asin,
    /// acos(x) → f64 — inverse cosine in radians (float.md §8). Transcendental, exempted; same
    /// domain [-1, 1] as asin.
    Acos,
    /// atan(x) → f64 — inverse tangent in radians (float.md §8). Transcendental, exempted; no
    /// domain restriction (atan(±Inf) = ±π/2).
    Atan,
    /// atan2(y, x) → f64 — quadrant-aware inverse tangent of y/x (float.md §8). Transcendental,
    /// exempted; two float operands (the resolver widens both to f64), no domain trap.
    Atan2,
    /// cot(x) → f64 — the cotangent, 1/tan(x) (float.md §8). Transcendental, exempted; cot(0) =
    /// +Infinity (no trap).
    Cot,
    /// Hyperbolic functions (float.md §8). Transcendental, exempted. sinh/cosh/tanh/asinh have no
    /// domain trap (sinh/cosh overflow to ±Inf, PG-faithful); acosh traps below 1, atanh outside
    /// [-1, 1] (but atanh(±1) = ±Inf is admissible).
    Sinh,
    Cosh,
    Tanh,
    Asinh,
    Acosh,
    Atanh,
    /// sign(x) → -1 / 0 / +1 (float.md §8). Two overloads: decimal → numeric (scale 0), float →
    /// f64 (EXACT/in-contract; sign(NaN) = sign(±0) = 0, sign(±Inf) = ±1). Dispatches on the
    /// operand value, like abs.
    Sign,
    /// div(a, b) → numeric — the TRUNCATED (toward zero) integer quotient of two numerics, at
    /// scale 0 (PG div(numeric, numeric)). Computed exactly as (a − a%b)/b so it is EXACT/in-contract.
    /// Resolver-routed (the catalog name "div" is taken by the `/` operator), accepts integer +
    /// decimal operands (integers promote), 22012 on a zero divisor.
    Div,
    /// gcd(a, b) → the greatest common divisor (non-negative), EXACT/in-contract. Integer operands →
    /// the promoted integer type (Euclid; a result whose magnitude overflows the type → 22003); a
    /// decimal operand → numeric at scale max(sₐ, s_b). gcd(0, 0) = 0. Resolver-routed.
    Gcd,
    /// lcm(a, b) → the least common multiple (non-negative), EXACT/in-contract, |a/gcd · b|.
    /// lcm(_, 0) = 0. Integer → the promoted type (overflow → 22003); decimal → numeric at scale
    /// max(sₐ, s_b). Resolver-routed (shares gcd's resolution).
    Lcm,
    /// factorial(n) → numeric — n! at scale 0 (PG factorial(bigint)). A negative operand → 22003.
    /// The O(n) multiply loop is metered per step (decimal_work, guarded) so the cost ceiling bounds
    /// a large factorial before the limb work runs (§13).
    Factorial,
    /// width_bucket(op, low, high, count) → i32 — the equi-width histogram bucket. Two overloads
    /// (numeric exact, float in f64); dispatches on the operand value. 2201G on a bad count / equal
    /// bounds (and, for float, a NaN operand / infinite bound); a result past int4 → 22003.
    WidthBucket,
    /// scale(numeric) → i32 — the decimal's display (fractional-digit) scale (decimal.md).
    Scale,
    /// min_scale(numeric) → i32 — the smallest scale that represents the value exactly (trailing
    /// fractional zeros dropped); zero has min_scale 0 (decimal.md).
    MinScale,
    /// trim_scale(numeric) → numeric — the value re-scaled down to its min_scale (trailing zeros
    /// removed), value-identical (decimal.md).
    TrimScale,
    /// make_interval — builds an interval from its (named/defaulted) integer components plus the
    /// f64 `secs` (spec/design/functions.md §11). The one scalar function returning interval.
    MakeInterval,
    /// make_timestamp(year, month, mday, hour, min, sec) → timestamp — the make_interval sibling
    /// (§11): every parameter named (none defaulted), the wall clock assembled from the fields.
    MakeTimestamp,
    /// make_timestamptz(year, month, mday, hour, min, sec[, timezone]) → timestamptz (§11) — as
    /// make_timestamp, then interprets the wall clock in the session zone (6-arg) or the explicit
    /// `timezone` text (7-arg), charging one `timezone` unit.
    MakeTimestamptz,
    /// make_date(year, month, day) → date — the MakeTimestamp sibling (functions.md §11); a
    /// negative year is BC, year zero / bad fields trap 22008.
    MakeDate,
    /// CURRENT_DATE (parser-desugared, also callable — functions.md §12, date.md §6): the
    /// statement clock's day in the session zone — the 'today' literal as a function. STABLE;
    /// charges one timezone unit beyond operator_eval.
    CurrentDate,
    /// date_part(field, source) → f64 — the float8-returning EXTRACT twin (timezones.md §9.2):
    /// the shared extract kernel, then decimal → f64. The field is a RUNTIME text value validated
    /// per row; a date source WIDENS TO MIDNIGHT (the timestamp matrix applies — PG's own
    /// definition); a timestamptz source decomposes in the session zone.
    DatePart,
    /// to_char(x, template) → text — PostgreSQL's data-type formatting renderer
    /// (spec/design/formatting.md §3, §6) over timestamp / timestamptz / interval / decimal / float
    /// and the int4 integer overload (i16 / i32); the overload is recovered from the value. A
    /// growth kernel (cost.md §8.1); a timestamptz is decomposed in the session zone.
    ToChar,
    /// to_char(i64, template) → text — PostgreSQL's int8 overload (its EEEE goes through numeric).
    ToCharInt8,
    /// to_timestamp(text, template) → timestamptz (formatting.md §5), or to_timestamp(f64) →
    /// timestamptz (Unix seconds); the arity picks the form.
    ToTimestamp,
    /// to_date(text, template) → date (formatting.md §5).
    ToDate,
    /// to_number(text, template) → decimal (formatting.md §7).
    ToNumber,
    /// justify_hours / justify_days / justify_interval(interval) → interval — PostgreSQL's interval
    /// normalizers (spec/design/interval.md §5.1); a field beyond i32 traps 22008.
    JustifyHours,
    JustifyDays,
    JustifyInterval,
    /// age(timestamp[tz], timestamp[tz]) → interval, and the one-argument age(x) =
    /// age(CURRENT_DATE at midnight, x) — PostgreSQL's symbolic field-wise difference with borrow
    /// (spec/design/interval.md §5.2). A timestamptz decomposes in the session zone; the one-arg
    /// form reads the statement clock (STABLE). Any ±infinity operand traps 22008.
    Age,
    /// uuid_extract_version(uuid) → i16 — the version nibble, NULL off-RFC-variant (§12).
    UuidExtractVersion,
    /// uuid_extract_timestamp(uuid) → timestamptz — the embedded instant for v1/v7, else NULL (§12).
    UuidExtractTimestamp,
    /// uuidv4() → uuid — random (the entropy seam, spec/design/entropy.md §3). VOLATILE.
    Uuidv4,
    /// uuidv7([interval]) → uuid — ms timestamp + monotonic counter + random (the entropy+clock
    /// seam, entropy.md §3); the optional interval shifts the embedded instant. VOLATILE.
    Uuidv7,
    /// now() → timestamptz — the statement clock (the clock seam, entropy.md §5), read ONCE and
    /// reused for the whole statement. STABLE. `current_timestamp` is parser sugar for this.
    Now,
    /// clock_timestamp() → timestamptz — the clock seam read on EVERY call, so it may advance
    /// within a statement (entropy.md §5). VOLATILE.
    ClockTimestamp,
    /// nextval(text) → i64 — advance the named sequence and return the new value
    /// (spec/design/sequences.md §4). VOLATILE; MUTATES the working snapshot (via `pending_seq`),
    /// so a statement calling it runs on the write path.
    Nextval,
    /// currval(text) → i64 — the value `nextval`/`setval` last produced for the named sequence IN
    /// THIS SESSION (sequences.md §6). VOLATILE; reads per-session state, 55000 before defined.
    Currval,
    /// setval(text, i64[, bool]) → i64 — set the named sequence's counter to the value and
    /// return it (sequences.md §4). VOLATILE; MUTATES the working snapshot, so a statement calling
    /// it runs on the write path. Arity 2 (is_called defaults true) or 3.
    Setval,
    /// lastval() → i64 — the value the most recent `nextval` (any sequence) returned IN THIS
    /// SESSION (sequences.md §6). VOLATILE; reads per-session state, 55000 before the first nextval.
    Lastval,
    /// current_setting(text[, bool]) → text — the named session variable's value (spec/design/session.md
    /// §6.1). STABLE; reads per-session state (the variable map). An unset name is `42704` unless the
    /// two-arg `missing_ok` is true (→ NULL). Arity 1 or 2.
    CurrentSetting,
    // json/jsonb processing functions (B1, spec/design/json-sql-functions.md §2). The `Json*` and
    // `Jsonb*` variants share a kernel; the only difference is the json overload parses the verbatim
    // text first. All propagate a SQL NULL input.
    /// json[b]_typeof → the JSON type name (object/array/string/number/boolean/null).
    JsonbTypeof,
    JsonTypeof,
    /// json[b]_array_length → the array element count; a non-array is 22023.
    JsonbArrayLength,
    JsonArrayLength,
    /// json[b]_strip_nulls → recursively remove object members whose value is JSON null.
    JsonbStripNulls,
    JsonStripNulls,
    /// jsonb_pretty → an indented multi-line render.
    JsonbPretty,
    /// JSON_SERIALIZE(json|jsonb) → the value's text serialization (json verbatim, jsonb canonical).
    JsonSerialize,
    // --- string / text functions (spec/design/string-functions.md). All STRICT (NULL propagates,
    // handled by the generic ScalarFunc null short-circuit). Character functions count Unicode code
    // points (`chars()`); octet/bit functions count UTF-8 bytes.
    /// length(text) → i32 — the number of characters (code points). length('héllo') = 5. Over
    /// bytea, the number of bytes (§4).
    Length,
    /// octet_length(text) → i32 — the number of UTF-8 bytes. octet_length('héllo') = 6. Over bytea,
    /// the number of bytes (§4).
    OctetLength,
    /// bit_length(text) → i32 — the number of UTF-8 bits = octet_length × 8. bit_length('héllo') = 48.
    /// Over bytea, bytes × 8 (§4).
    BitLength,
    /// substr / substring(text, start[, count]) → text — the function form of SUBSTRING (1-based,
    /// code-point indexed); over bytea the same window over bytes. A negative count is 22011
    /// (string-functions.md §3/§4).
    Substr,
    /// left(text, n) → text — the first n characters; a negative n drops the last |n| (§3).
    Left,
    /// right(text, n) → text — the last n characters; a negative n drops the first |n| (§3).
    Right,
    /// lpad(text, length[, fill]) → text — left-pad to `length` chars with `fill` (default space);
    /// a longer string truncates; an over-large length traps 54000 (§3).
    Lpad,
    /// rpad(text, length[, fill]) → text — the right-hand mirror of lpad (§3).
    Rpad,
    /// btrim(text[, chars]) → text — trim characters in the `chars` set from both ends (§3).
    Btrim,
    /// ltrim(text[, chars]) → text — trim the `chars` set from the LEADING end only (§3).
    Ltrim,
    /// rtrim(text[, chars]) → text — trim the `chars` set from the TRAILING end only (§3).
    Rtrim,
    /// replace(text, from, to) → text — replace every occurrence of substring `from` with `to` (§3).
    Replace,
    /// translate(text, from, to) → text — per-character map/delete by position in `from`/`to` (§3).
    Translate,
    /// repeat(text, n) → text — the string concatenated n times; over-large result traps 54000 (§3).
    Repeat,
    /// reverse(text) → text — the code points in reverse order (§3).
    Reverse,
    /// strpos(text, substring) → i32 — 1-based code-point position of the first match, else 0 (§3).
    /// `position(string, substring)` (the POSITION(sub IN s) special form's call) shares it, over
    /// text and bytea (byte position, §4).
    Strpos,
    /// split_part(text, delimiter, n) → text — the n-th field of the split; n=0 traps 22023 (§3).
    SplitPart,
    /// starts_with(text, prefix) → boolean — true iff the string begins with `prefix` (§3).
    StartsWith,
    /// ascii(text) → i32 — the Unicode code point of the first character; empty → 0 (§3).
    Ascii,
    /// chr(int) → text — the one-character string for a Unicode code point; bad point traps (§3).
    Chr,
    /// initcap(text) → text — titlecase each word (ASCII word boundaries + ASCII case fold, §3).
    Initcap,
    /// to_hex(int) → text — lowercase hex of the value's 64-bit two's-complement pattern (§3).
    ToHex,
    /// encode(bytea, format) → text — render bytes as hex / base64 / escape (§3).
    Encode,
    /// decode(text, format) → bytea — parse hex / base64 / escape back to binary (§3).
    Decode,
    /// get_byte(bytea, n) → i32 — the n-th byte (0-based); out of range traps 2202E (§4).
    GetByte,
    /// set_byte(bytea, n, v) → bytea — a copy with the n-th byte set to v's low 8 bits; 2202E (§4).
    SetByte,
    /// get_bit(bytea, n) → i32 — the n-th bit (0-based, LSB first in each byte); 2202E (§4).
    GetBit,
    /// set_bit(bytea, n, v) → bytea — a copy with the n-th bit set to v (0/1, else 22023); 2202E (§4).
    SetBit,
    /// md5(text | bytea) → text — the MD5 digest as 32 lowercase hex digits; text hashes its UTF-8
    /// bytes (§4).
    Md5,
    /// sha224(bytea) → bytea — the 28-byte SHA-224 digest (§4).
    Sha224,
    /// sha256(bytea) → bytea — the 32-byte SHA-256 digest (§4).
    Sha256,
    /// sha384(bytea) → bytea — the 48-byte SHA-384 digest (§4).
    Sha384,
    /// sha512(bytea) → bytea — the 64-byte SHA-512 digest (§4).
    Sha512,
    /// overlay(string, new, start[, count]) → text / bytea — the SQL OVERLAY splice: count units from
    /// start replaced by new (count defaults to length(new)); start ≤ 0 is 22011, a start + count
    /// past int4 is 22003. A growth kernel (cost.md §8.1; string-functions.md §3/§4).
    Overlay,
    /// bytea || bytea → bytea — the operands' bytes appended (§4). Reached only through the `||`
    /// resolver (it has no function-call name); a growth kernel (cost.md §8.1).
    ByteaConcat,
    /// text || text → text — the operands' characters appended (§5). Reached only through the `||`
    /// resolver, which renders a non-text operand through its text cast; a growth kernel (cost.md
    /// §8.1).
    TextConcat,
    /// quote_literal(text) → text — wrap as a SQL string literal (§3).
    QuoteLiteral,
    /// quote_ident(text) → text — wrap as a SQL identifier (§3).
    QuoteIdent,
    /// quote_nullable(text) → text — like quote_literal but NON-STRICT (NULL → 'NULL', §3).
    QuoteNullable,
    /// like_escape(pattern, escape) → text — rewrite a LIKE pattern to the default `\` escape; what
    /// `LIKE … ESCAPE` desugars to (§3, grammar.md §22).
    LikeEscape,
    /// similar_to_escape(pattern [, escape]) → text — PostgreSQL's SIMILAR-to-regex translation; what
    /// `SIMILAR TO … [ESCAPE …]` desugars to (regex.md §8f).
    SimilarToEscape,
}

/// The polymorphic array functions (spec/design/array-functions.md). Distinct from
/// [`ScalarFunc`] because they resolve over the `anyarray`/`anyelement` pseudo-families (§2) and
/// the builders return an *array* type (not a `ScalarType`), so they get their own resolved node
/// ([`RExpr::ArrayFunc`]). The kernel id is the function name; the eval recovers everything else
/// from the operand values (the array's own shape header), so the node carries no result type.
#[derive(Debug)]
pub(crate) enum ArrayFunc {
    /// array_ndims(anyarray) → i32 — the dimension count; NULL for the empty array.
    ArrayNdims,
    /// array_length(anyarray, integer) → i32 — length of a dimension; NULL if empty / out of range.
    ArrayLength,
    /// array_lower(anyarray, integer) → i32 — a dimension's lower bound; NULL if empty / out of range.
    ArrayLower,
    /// array_upper(anyarray, integer) → i32 — a dimension's upper bound; NULL if empty / out of range.
    ArrayUpper,
    /// cardinality(anyarray) → i32 — the total element count; 0 for the empty array.
    Cardinality,
    /// array_dims(anyarray) → text — the bound spec `[l1:u1][l2:u2]…`; NULL for the empty array.
    ArrayDims,
    /// array_append(anyarray, anyelement) → anyarray — non-strict; NULL/empty array → `{e}`;
    /// a multidimensional array is 22000 (§3.2).
    ArrayAppend,
    /// array_prepend(anyelement, anyarray) → anyarray — the mirror of array_append.
    ArrayPrepend,
    /// array_cat(anyarray, anyarray) → anyarray — non-strict identity-aware concatenation along
    /// the outer dimension; incompatible dimensionalities are 2202E (§3.2).
    ArrayCat,
    /// array_remove(anyarray, anyelement) → anyarray — drop every element NOT DISTINCT FROM the
    /// value (1-D/empty only — a multidimensional array is 0A000); the lower bound is preserved (§8).
    ArrayRemove,
    /// array_replace(anyarray, anyelement, anyelement) → anyarray — substitute every element NOT
    /// DISTINCT FROM `from` with `to`; any dimensionality, shape preserved (§8).
    ArrayReplace,
    /// array_position(anyarray, anyelement[, integer]) → i32 — the first match's SUBSCRIPT (in the
    /// array's lower-bound space), NULL if absent; 1-D/empty only (0A000); the optional `start`
    /// subscript begins the scan and a NULL `start` is 22004 (§8).
    ArrayPosition,
    /// array_positions(anyarray, anyelement) → i32[] — the i32[] of every match's subscript
    /// (empty {} if none); 1-D/empty only (0A000) (§8).
    ArrayPositions,
    /// array_to_string(anyarray, text[, text]) → text — the elements' output text joined by the
    /// separator; NULL elements skipped or replaced by the null string (§15). An amplifier.
    ArrayToString,
    /// string_to_array(text, text[, text]) → text[] — the string split at the separator (a NULL
    /// separator splits into characters); pieces equal to the null string become NULL (§15).
    StringToArray,
    /// `a @> b` (anyarray, anyarray) → boolean — does `a` CONTAIN `b`: is every element of `b`
    /// present in `a` (STRICT equality, NULL matches nothing) over the flattened multiset, any
    /// dimensionality (§10). A NULL whole-array operand → NULL.
    Contains,
    /// `a <@ b` (anyarray, anyarray) → boolean — is `a` CONTAINED BY `b` (i.e. `b @> a`) (§10).
    ContainedBy,
    /// `a && b` (anyarray, anyarray) → boolean — do `a` and `b` OVERLAP: share at least one element
    /// (STRICT equality) over the flattened multiset, any dimensionality (§10).
    Overlaps,
}

/// The polymorphic range ACCESSOR functions (spec/design/range-functions.md §1, RF1). Like
/// [`ArrayFunc`], they resolve over a pseudo-family (`anyrange`, binding ELEM := the element type)
/// and get their own resolved node ([`RExpr::RangeFunc`]); the kernel recovers everything from the
/// operand range value (self-describing). All are STRICT (a NULL range → NULL). `lower`/`upper`
/// return the bound value (ELEM) or NULL when empty/unbounded; the rest return boolean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RangeFunc {
    /// lower(anyrange) → anyelement — the lower bound value; NULL if the range is empty or
    /// unbounded below.
    Lower,
    /// upper(anyrange) → anyelement — the upper bound value; NULL if empty or unbounded above.
    Upper,
    /// isempty(anyrange) → boolean — is this the empty range.
    IsEmpty,
    /// lower_inc(anyrange) → boolean — is the lower bound inclusive (always false for empty / an
    /// infinite lower bound).
    LowerInc,
    /// upper_inc(anyrange) → boolean — is the upper bound inclusive (always false for empty / an
    /// infinite upper bound).
    UpperInc,
    /// lower_inf(anyrange) → boolean — is the lower bound infinite (false for the empty range).
    LowerInf,
    /// upper_inf(anyrange) → boolean — is the upper bound infinite (false for the empty range).
    UpperInf,
}

/// The regular-expression scalar functions (spec/design/regex.md §8). The kernel id; the eval
/// recovers the arg shape (3/4 for replace, 2/3 for match) from `args.len()`. Kernels in `regex.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegexFunc {
    /// regexp_replace(source, pattern, replacement [, flags]) → text.
    Replace,
    /// regexp_match(source, pattern [, flags]) → text[].
    Match,
    /// regexp_like(string, pattern [, flags]) → boolean (regex.md §8b).
    Like,
    /// regexp_count(string, pattern [, start [, flags]]) → integer (regex.md §8b).
    Count,
    /// regexp_substr(string, pattern [, start [, N [, flags [, subexpr]]]]) → text (regex.md §8b).
    Substr,
    /// substring(string, pattern) / `SUBSTRING(string FROM pattern)` → text (regex.md §8d): the
    /// first match's first capture group when the pattern has one, else the whole match.
    Substring,
    /// substring(string, pattern, escape) / `SUBSTRING(string SIMILAR pattern ESCAPE escape)` → text
    /// (regex.md §8f.3): the text between the pattern's two separators, the pattern matching the
    /// whole string. Its programs live in the node's `similar` matcher, not `program`.
    SimilarSubstring,
    /// regexp_instr(string, pattern [, start [, N [, endoption [, flags [, subexpr]]]]]) → integer
    /// (regex.md §8b).
    Instr,
    /// regexp_split_to_array(string, pattern [, flags]) → text[] (regex.md §8c).
    SplitToArray,
    /// regexp_matches(string, pattern [, flags]) — the set-returning form (regex.md §8c), reached
    /// only as the single arg of a [`SrfKind::RegexpMatches`] relation. Its eval yields an internal
    /// one-dimensional array whose elements are the per-match `text[]` capture arrays (one per
    /// produced row; empty for no match) — a transport the SRF producer unpacks, never a SQL value.
    Matches,
    /// regexp_split_to_table(string, pattern [, flags]) — the set-returning split (regex.md §8c),
    /// reached only as the single arg of a [`SrfKind::RegexpSplitToTable`] relation. Its eval yields
    /// the `regexp_split_to_array` pieces, which the SRF producer unpacks one row each.
    SplitToTable,
}

/// The range BOOLEAN operators (spec/design/range-functions.md §3, RF3). Each is a binary infix
/// operator returning a definite boolean (a NULL operand short-circuits to NULL at eval, like the
/// array containment operators). `ContainsElem`/`ElemContainedBy` are the element overloads of
/// `@>`/`<@` (the other operand is a bare element coerced to the range's element type); the rest are
/// range-against-range. The kernels live in `range.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RangeOp {
    /// `a @> b` — range `a` contains range `b`.
    Contains,
    /// `r @> e` — range `r` contains element `e` (the element overload of `@>`).
    ContainsElem,
    /// `a <@ b` — range `a` is contained by range `b`.
    ContainedBy,
    /// `e <@ r` — element `e` is contained by range `r` (the element overload of `<@`).
    ElemContainedBy,
    /// `a && b` — ranges `a` and `b` overlap.
    Overlaps,
    /// `a << b` — `a` is strictly left of `b`.
    Before,
    /// `a >> b` — `a` is strictly right of `b`.
    After,
    /// `a &< b` — `a` does not extend to the right of `b`.
    Overleft,
    /// `a &> b` — `a` does not extend to the left of `b`.
    Overright,
    /// `a -|- b` — `a` and `b` are adjacent.
    Adjacent,
}

/// The range SET operators (spec/design/range-functions.md §4, RF4). Each combines two ranges over a
/// common element type into a new range (`RExpr::RangeSetOp`). `Union`/`Difference` raise `22000` on a
/// non-contiguous result; `Intersect`/`Merge` never error. The kernels live in `range.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RangeSetOp {
    /// `a + b` — union: the smallest single range covering both (22000 if they leave a gap).
    Union,
    /// `a * b` — intersection: the overlap (empty when the ranges are disjoint).
    Intersect,
    /// `a - b` — difference: the part of `a` not in `b` (22000 if `b` splits `a` in two).
    Difference,
    /// `range_merge(a, b)` — like union but spans any gap between the ranges silently (never errors).
    Merge,
}

/// The VARIADIC argument-counting functions (spec/design/array-functions.md §12). Distinct from
/// [`ScalarFunc`] because they are non-strict (`null = "none"`, like [`ArrayFunc`]) and take either
/// a spread of arguments or a single array via the `VARIADIC` keyword — the call form is carried on
/// the [`RExpr::Variadic`] node. Both return `i32`.
#[derive(Debug)]
pub(crate) enum VariadicFunc {
    /// num_nulls(VARIADIC "any") → i32 — the count of NULL arguments (spread form), or of NULL
    /// flattened elements (VARIADIC-array form; a NULL whole-array operand → NULL). Never NULL in
    /// spread form.
    NumNulls,
    /// num_nonnulls(VARIADIC "any") → i32 — the mirror: the count of non-NULL arguments/elements.
    NumNonnulls,
    /// concat(VARIADIC "any") → text — the non-NULL arguments' output text, joined (string-functions.md §6).
    Concat,
    /// concat_ws(sep, VARIADIC "any") → text — the non-NULL arguments joined by `sep` (NULL sep → NULL).
    ConcatWs,
    /// format(formatstr, VARIADIC "any") → text — PostgreSQL's printf-style %s / %I / %L.
    Format,
}

/// Which scalar jsonpath query function an [`RExpr::JsonPathFn`] node is (jsonpath.md §5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JsonPathFnKind {
    /// `jsonb_path_exists` → boolean (the sequence is non-empty).
    Exists,
    /// The PostgreSQL `@?` operator: the same existence test in silent mode — a SQL/JSON error
    /// yields SQL NULL.
    ExistsSilent,
    /// `jsonb_path_query_first` → the first sequence item, or NULL if empty.
    QueryFirst,
    /// `jsonb_path_query_array` → the sequence wrapped in a JSON array.
    QueryArray,
    /// `jsonb_path_match` → the single boolean the path/predicate produces (22038 if not exactly one
    /// boolean item).
    Match,
    /// The PostgreSQL `@@` operator: the same match with non-singleton/non-boolean results suppressed
    /// to SQL NULL.
    MatchSilent,
}

/// Which SQL/JSON query function an [`RExpr::JsonSqlFn`] node is (json-sql-functions.md §5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JsonSqlKind {
    /// `JSON_EXISTS` → boolean (non-empty sequence); errors honor ON ERROR (default FALSE).
    Exists,
    /// `JSON_VALUE` → a single scalar coerced to the RETURNING type (default text).
    Value,
    /// `JSON_QUERY` → a json/jsonb value (wrapper / quotes controlled).
    Query,
}

/// Which json/jsonb builder an [`RExpr::JsonBuild`] node is (json-sql-functions.md §2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JsonBuildKind {
    /// json[b]_build_array — every argument is one array element (NULL → JSON null).
    Array,
    /// json[b]_build_object — alternating key/value arguments (odd count / NULL key → 22023).
    Object,
}

/// Which JSON-image function an [`RExpr::JsonImage`] node is (json-sql-functions.md §2.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JsonImageFunc {
    /// `to_json(anyelement)` → the value's `json` image.
    ToJson,
    /// `to_jsonb(anyelement)` → the value's `jsonb` image.
    ToJsonb,
    /// `array_to_json(anyarray [, pretty])` → the array's `json` image.
    ArrayToJson,
    /// `row_to_json(record [, pretty])` → the composite's `json` object.
    RowToJson,
    /// `JSON_SCALAR(expr)` → the value's `json` image, as `to_json` (PG's JSCTOR_JSON_SCALAR); a bare
    /// string literal is `text`.
    JsonScalar,
}

/// One resolved subscript spec in an [`RExpr::Subscript`] (spec/design/array.md §6): a single
/// index `a[i]`, or a slice `a[m:n]` whose bounds may be omitted (`a[:n]`, `a[m:]`, `a[:]`).
pub(crate) enum RSubscript {
    Index(Box<RExpr>),
    Slice {
        lower: Option<Box<RExpr>>,
        upper: Option<Box<RExpr>>,
    },
}

/// A resolved expression: a tree over fixed column indices, ready to evaluate against
/// a row. Arithmetic nodes carry their (promotion-tower) result type so the computed
/// value can be range-checked against it (the i16+i16 → i16 boundary).
/// Which jsonb delete form a [`RExpr::JsonDelete`] applies (json-sql-functions.md §1, J6).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DeleteKind {
    /// `jsonb - text` — delete a key (object) or matching string elements (array).
    Key,
    /// `jsonb - int` — delete the array element at an index.
    Index,
    /// `jsonb - text[]` — delete each key.
    Keys,
    /// `jsonb #- text[]` — delete the element at a path.
    Path,
}

/// Which jsonb key-existence operator a [`RExpr::JsonHasKey`] applies (json-sql-functions.md §1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HasKeyKind {
    /// `?` — a single key (text) exists.
    One,
    /// `?|` — any key of a `text[]` exists.
    Any,
    /// `?&` — all keys of a `text[]` exist.
    All,
}

/// Which jsonb accessor operator a [`RExpr::JsonGet`] applies (spec/design/json-sql-functions.md §1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JsonGetOp {
    /// `->` — field by key (text arg) or element by index (integer arg); result jsonb.
    Arrow,
    /// `->>` — same access, rendered as text.
    ArrowText,
    /// `#>` — get at a `text[]` path; result jsonb.
    HashArrow,
    /// `#>>` — get at a `text[]` path, rendered as text.
    HashArrowText,
}

pub(crate) enum RExpr {
    Column(usize),
    ConstInt(i64),
    ConstBool(bool),
    ConstText(String),
    ConstDecimal(Decimal),
    /// A `f32` / `f64` constant (a typed float literal, an adapted decimal/int literal
    /// in a float context, or a folded subquery value — spec/design/float.md §4).
    ConstFloat32(f32),
    ConstFloat64(f64),
    ConstBytea(Vec<u8>),
    ConstUuid([u8; 16]),
    /// A `json` constant — JSON text stored VERBATIM (spec/design/json.md §4), validated
    /// well-formed at resolve.
    ConstJson(String),
    /// A `jsonb` constant — the canonical tagged-node tree (spec/design/json.md §2), parsed +
    /// canonicalized at resolve. Boxed to keep `RExpr` small (a `JsonNode` is a recursive tree).
    ConstJsonb(Box<JsonNode>),
    /// A `jsonpath` constant — the canonical normalized source text (spec/design/jsonpath.md, P1a),
    /// compiled + rendered at resolve.
    ConstJsonPath(String),
    /// A parsed `timestamp` / `timestamptz` literal: the i64 microsecond instant.
    ConstTimestamp(i64),
    ConstTimestamptz(i64),
    /// A parsed `date` literal: the i32 day count since 1970-01-01 (spec/design/date.md).
    ConstDate(i32),
    /// A parsed `interval` literal: the three-field span (spec/design/interval.md).
    ConstInterval(Interval),
    ConstNull,
    /// A `ROW(...)` constructor (spec/design/composite.md §1): evaluate each field expression and
    /// assemble a `Value::Composite`. Also the folded form of a composite constant (`value_to_rexpr`
    /// wraps each field's constant). One `operator_eval` per node (cost.md §9).
    Row(Vec<RExpr>),
    /// An `ARRAY[…]` constructor (spec/design/array.md §1): evaluate each element expression
    /// (coercing to the unified element type) and assemble a `Value::Array`. When `nested` is true
    /// the elements are themselves arrays and are **stacked** into a value of one higher dimension
    /// (all sub-arrays must share dims/lbounds — else `2202E`); otherwise it is a flat 1-D array
    /// (lower bound 1). One `operator_eval` per node.
    Array {
        elems: Vec<RExpr>,
        nested: bool,
    },
    /// A constant array value (the folded form of an array constant — `value_to_rexpr` — preserving
    /// its shape). Boxed so the (rarely-used) shaped payload does not widen every `RExpr` frame.
    /// Eval returns it directly.
    ConstArray(Box<ArrayVal>),
    /// A constant range value (the folded form of a range constant — `'[1,5)'::i32range`, already
    /// canonicalized at resolve). Boxed so the payload does not widen every `RExpr` frame. Eval
    /// returns it directly.
    ConstRange(Box<RangeVal>),
    /// Field selection `(composite).field` (spec/design/composite.md §S4): evaluate `base` to a
    /// composite value and return its `index`-th field (the field ordinal, fixed at resolve). A
    /// whole-value-NULL composite yields NULL for any field. One `operator_eval` per node.
    Field {
        base: Box<RExpr>,
        index: usize,
    },
    /// Array subscript `base[..][..]` (spec/design/array.md §6): one or more subscript specs applied
    /// to `base`. If `is_slice` is false every spec is an index and the node reads a single element
    /// (the element type) — NULL when the subscript count ≠ ndim or any index is out of range. If
    /// `is_slice` is true the node returns a sub-array (the array type); a scalar index `i` in that
    /// context means `1:i`. A NULL array or any NULL bound yields NULL. One `operator_eval` per node.
    Subscript {
        base: Box<RExpr>,
        subscripts: Vec<RSubscript>,
        is_slice: bool,
    },
    /// A bind parameter, by 0-based index into the bound-values slice passed to `eval`. Its
    /// static type was inferred from context at resolve (spec/design/api.md §5); the value
    /// is supplied (and coerced to that type) before evaluation.
    Param(usize),
    Cast {
        inner: Box<RExpr>,
        target: ScalarType,
        /// For a decimal target, the optional `numeric(p,s)` typmod to coerce to.
        typmod: Option<DecimalTypmod>,
        /// For a `varchar(n)` target (a `text` cast with a length), the max length to
        /// **truncate** to — an explicit cast silently truncates, never raising 22001
        /// (spec/design/types.md §15). `None` for any non-text / unbounded target.
        varchar_len: Option<u32>,
        /// The cast's trap classification (functions.md §15), recorded at resolve from its source
        /// and target types (casts.toml); `May` unless classified. Read only by the planner's
        /// safe-conjunct gate (planner.md §3.5).
        trap: TrapClass,
        /// For a `timestamp(p)` / `timestamptz(p)` / qualified interval target, the typmod the
        /// result is rounded / cut to — PostgreSQL's `timestamp_scale` / `interval_scale` — and,
        /// for a text source, the qualifier the interval text is read under
        /// (spec/design/timestamp.md §8, interval.md §6.1). `None` for every other target.
        temporal: Option<TemporalTypmod>,
    },
    /// A cast that *involves* an array type (spec/design/array.md §7) — the three follow-on array
    /// casts, none expressible by the scalar [`RExpr::Cast`] node (whose `target` is a `ScalarType`):
    /// runtime `text → T[]` (`array_in` per row), `array → text` (`array_out` per row), and
    /// element-wise `array → other-element-array` (each element through the scalar cast). `to_elem`
    /// is `Some(target element ColType)` for the two array-producing casts (text→array, array→array)
    /// and `None` for `array → text`. The eval branches on the runtime value (Text vs Array).
    ArrayCast {
        inner: Box<RExpr>,
        to_elem: Option<ColType>,
    },
    /// A runtime cast that *involves* a composite type (spec/design/composite.md §8): `text →
    /// composite` (`record_in` per row, `to = Some(the composite's ColType)`) and `composite → text`
    /// (`record_out` per row, `to = None`). The eval branches on the runtime value (Text vs
    /// Composite). A `ROW(…)::type` cast needs no node — its fields are cast individually.
    CompositeCast {
        inner: Box<RExpr>,
        to: Option<ColType>,
    },
    Neg {
        operand: Box<RExpr>,
        result: ScalarType,
    },
    Not(Box<RExpr>),
    Arith {
        op: ArithOp,
        lhs: Box<RExpr>,
        rhs: Box<RExpr>,
        result: ScalarType,
    },
    Compare {
        op: CmpOp,
        lhs: Box<RExpr>,
        rhs: Box<RExpr>,
        /// The derived collation for this comparison (spec/design/collation.md §7). `None` is the
        /// `C` / default byte order (the unchanged fast path); `Some` is a loaded non-`C` collation
        /// that orders the ORDERING comparisons (`< <= > >=`) by its UCA sort key. `=`/`<>` are
        /// byte-equality regardless (deterministic-collation equality IS byte-identity, §7), so the
        /// collation only changes the ordering ops at eval — but it is derived (and conflict-checked,
        /// 42P22) for every comparison op.
        collation: Option<std::sync::Arc<Collation>>,
    },
    And(Box<RExpr>, Box<RExpr>),
    Or(Box<RExpr>, Box<RExpr>),
    /// A jsonb accessor operator (`-> ->> #> #>>`, spec/design/json-sql-functions.md §1, J4). `op`
    /// selects field/index vs path and text-vs-jsonb; `base` evaluates to a jsonb document; `arg` is
    /// the key (text), array index (integer), or path (`text[]`). The result is jsonb (`-> #>`) or
    /// text (`->> #>>`), and is SQL NULL when the access misses (or when base/arg is NULL).
    JsonGet {
        op: JsonGetOp,
        base: Box<RExpr>,
        arg: Box<RExpr>,
    },
    /// `a @> b` jsonb deep containment (spec/design/json-sql-functions.md §1, J5) — does `a` contain
    /// `b`. `<@` resolves to this with the operands swapped. Boolean; strict (a NULL operand → NULL).
    JsonContains {
        a: Box<RExpr>,
        b: Box<RExpr>,
    },
    /// `jsonb ? text` / `?| text[]` / `?& text[]` key-existence (spec/design/json-sql-functions.md §1,
    /// J5). `kind` selects one-key / any-key / all-keys. Boolean; strict.
    JsonHasKey {
        kind: HasKeyKind,
        base: Box<RExpr>,
        arg: Box<RExpr>,
    },
    /// `a || b` jsonb concatenate / shallow-merge (spec/design/json-sql-functions.md §1, J6). Result
    /// jsonb; strict (a NULL operand → SQL NULL).
    JsonConcat {
        a: Box<RExpr>,
        b: Box<RExpr>,
    },
    /// `jsonb - text|int|text[]` (delete key/index/keys) and `jsonb #- text[]` (delete at path) —
    /// the J6 mutation deletes (spec/design/json-sql-functions.md §1). `kind` selects the form;
    /// `base` is the jsonb document, `arg` the key/index/key-array/path. Result jsonb; strict; a
    /// delete from a scalar (or an integer index into an object) is `22023`.
    JsonDelete {
        kind: DeleteKind,
        base: Box<RExpr>,
        arg: Box<RExpr>,
    },
    /// `jsonb_set` / `jsonb_insert` (json-sql-functions.md §2): a jsonb path mutation. `args` is
    /// `[target jsonb, path text[], value jsonb, flag boolean]` — STRICT (any NULL → SQL NULL).
    /// `mode` selects replace-or-create (Set) vs insert (Insert); the flag is create_if_missing
    /// (Set) / insert_after (Insert), defaulting to true / false respectively. `lax` is
    /// `jsonb_set_lax` (mode Set): a fifth `null_value_treatment text` arg, and NOT strict in the
    /// value — a SQL NULL value is handled by the treatment.
    JsonSetInsert {
        mode: json::PathSetMode,
        lax: bool,
        args: Vec<RExpr>,
    },
    /// `json_object` / `jsonb_object` (json-sql-functions.md §2): build an object from text array(s).
    /// `args` is one `text[]` of alternating keys/values, or two `text[]` (keys, values). The VALUES
    /// are always JSON strings (a NULL value → JSON null); a NULL key → 22004. STRICT in the whole
    /// array argument(s). `json` selects the json (insertion order + dups + " : " spacing) vs jsonb
    /// (canonical) result.
    JsonObjectFromArrays {
        json: bool,
        args: Vec<RExpr>,
    },
    /// A scalar jsonpath query function (P2, jsonpath.md §5): `jsonb_path_exists` /
    /// `jsonb_path_query_first` / `jsonb_path_query_array` / `jsonb_path_match` (`args` = `[target
    /// jsonb, path jsonpath, vars jsonb, silent boolean]`, `tz` for the `_tz` form) or the `@?` /
    /// `@@` operator (`args` = `[target, path]`). STRICT (any NULL → SQL NULL). The path is
    /// recompiled from its canonical text at eval.
    JsonPathFn {
        kind: JsonPathFnKind,
        tz: bool,
        args: Vec<RExpr>,
    },
    /// A SQL/JSON query function `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY` (json-sql-functions.md
    /// §5, S2). `ctx` produces the context jsonb (or json/text, coerced), `path` the jsonpath; the
    /// behaviors / wrapper / quotes drive the result. The result type is fixed at resolve.
    JsonSqlFn {
        kind: JsonSqlKind,
        ctx: Box<RExpr>,
        path: Box<RExpr>,
        /// The `PASSING` values, each already a JSON-item-shaped value (see [`resolve_json_passing`]),
        /// and the variable names they bind, in clause order.
        /// Boxed slices, not Vecs: this variant sets `RExpr`'s size, which every recursive
        /// resolve/eval frame carries (the depth budget, cost.md §7.1).
        passing: Box<[RExpr]>,
        passing_names: Box<[String]>,
        /// The RETURNING scalar type (`Bool` for JSON_EXISTS; the JSON_VALUE scalar target;
        /// `Jsonb`/`Json` for JSON_QUERY) — drives the result coercion.
        returning: ScalarType,
        decimal: Option<DecimalTypmod>,
        wrapper: JsonWrapper,
        on_empty: JsonOnBehavior,
        on_error: JsonOnBehavior,
        /// The `DEFAULT expr` of `on_empty` / `on_error` when that behavior is
        /// [`JsonOnBehavior::Default`] (json-sql-functions.md §5.3): already coerced to the
        /// RETURNING type, or `jsonb` coerced as a JSON item when the behavior fires.
        on_empty_default: Option<Box<RExpr>>,
        on_error_default: Option<Box<RExpr>>,
    },
    IsNull {
        operand: Box<RExpr>,
        negated: bool,
    },
    /// `operand IS [NOT] JSON …` (json-sql-functions.md §5): well-formedness + optional kind /
    /// unique-keys test over a string / json / jsonb operand. A NULL operand → NULL; else a definite
    /// boolean (NOT-negated when `negated`).
    IsJson {
        operand: Box<RExpr>,
        negated: bool,
        kind: JsonPredicateKind,
        unique_keys: bool,
    },
    /// `JSON(text [(WITH|WITHOUT) UNIQUE [KEYS]])` (json-sql-functions.md §5): validate a string as a
    /// `json` value (verbatim). Malformed → 22P02; `WITH UNIQUE KEYS` on a duplicate key → 22030.
    /// STRICT (a NULL operand → SQL NULL).
    JsonCtor {
        operand: Box<RExpr>,
        unique_keys: bool,
    },
    /// `lhs IS [NOT] DISTINCT FROM rhs` — NULL-safe equality. `negated = true` is the
    /// `IS NOT DISTINCT FROM` ("are they the same?") form; `false` is `IS DISTINCT FROM`.
    /// Always evaluates to a definite boolean.
    Distinct {
        lhs: Box<RExpr>,
        rhs: Box<RExpr>,
        negated: bool,
    },
    /// `lhs LIKE rhs` / `lhs NOT LIKE rhs` — text pattern match (grammar.md §22). Both operands
    /// resolve to text (or NULL); a NULL operand makes the result NULL. The matcher runs over
    /// Unicode code points and traps 22025 on a pattern ending in a lone escape reached during
    /// matching. `negated` carries the NOT keyword (NOT LIKE = the negation of the match);
    /// `insensitive` carries `ILIKE` — both operands are simple-lowercased (collation.md §16)
    /// under the engine casing regime before matching.
    Like {
        lhs: Box<RExpr>,
        rhs: Box<RExpr>,
        negated: bool,
        insensitive: bool,
    },
    /// `lhs ~ rhs` / `~*` / `!~` / `!~*` — regular-expression match (regex.md). Both operands
    /// resolve to text (or NULL); a NULL operand makes the result NULL (before the matcher runs).
    /// Matched by the hand-written Pike VM (regex.rs) over Unicode code points; `negated` carries
    /// `!~`/`!~*`, `insensitive` carries `~*`/`!~*` (both operands simple-lowercased like ILIKE).
    /// `program` holds the precompiled NFA for a CONSTANT pattern (compiled once at resolve, the
    /// `col ~ 'literal'` case — regex.md §5); `None` means the pattern is non-constant and compiled
    /// per row at eval. `compile_charged` is the one-shot flag that charges a precompiled program's
    /// `regex_compile` cost once per statement execution (on first eval), not per row.
    Regex {
        lhs: Box<RExpr>,
        rhs: Box<RExpr>,
        negated: bool,
        insensitive: bool,
        /// `SIMILAR TO` (regex.md §8f.2): the pattern compiles in the dot-all mode.
        similar: bool,
        program: Option<Box<crate::regex::Program>>,
        compile_charged: std::cell::Cell<bool>,
    },
    /// `upper(text)` / `lower(text)` — Unicode case folding (collation.md §16). `upper` selects the
    /// direction. Folds via the engine-global property table when a bundle is loaded, else the ASCII
    /// baseline (fold `a–z`/`A–Z`, pass other code points through). A NULL operand propagates.
    Casing {
        upper: bool,
        arg: Box<RExpr>,
    },
    /// `value AT TIME ZONE zone` (grammar.md §49, timezones.md §6) — desugared from the operator and
    /// from a bare `timezone(zone, value)` call. `to_timestamptz` selects the direction: `false` is
    /// `timestamptz → timestamp` (render the UTC instant as the local wall clock in `zone`); `true`
    /// is `timestamp → timestamptz` (interpret the wall clock as in `zone`, producing the UTC
    /// instant). Reads the engine-global loaded zone set (timezone.rs); an unknown zone is `22023`,
    /// a NULL operand propagates. `±infinity` passes through unchanged.
    AtTimeZone {
        zone: Box<RExpr>,
        value: Box<RExpr>,
        to_timestamptz: bool,
    },
    /// `date_trunc(unit, value[, zone])` (timezones.md §9.1) — truncate `value` down to `unit`.
    /// `unit` is a runtime text expression (case-insensitive; an unrecognized unit is `22023` at
    /// eval). For a `timestamptz` `value` the truncation is in `zone` (the 3-arg form) or the session
    /// zone (`zone = None`, the 2-arg form), charging the `timezone` unit; for `timestamp`/`interval`
    /// it is zone-free. The result family is the `value` family (the runtime `Value` dispatches).
    DateTrunc {
        unit: Box<RExpr>,
        value: Box<RExpr>,
        zone: Option<Box<RExpr>>,
    },
    /// `EXTRACT(field FROM value)` (timezones.md §9.2) — the `numeric` value of `field` (lowercased,
    /// validated at resolve). For a `timestamptz` `value`, every field but `epoch` is computed in the
    /// session zone (charging `timezone`); the `timezone*` fields read the session offset.
    Extract {
        field: String,
        value: Box<RExpr>,
    },
    /// A cross-family datetime cast (timezones.md §9.3) to `to` (`timestamp`/`timestamptz`/`date`)
    /// from another datetime family — the runtime `Value` carries the source family — or the
    /// runtime text → date cast (date.md §6). Casts crossing the `timestamptz` boundary consult
    /// the session zone (charging `timezone`); `±infinity` and NULL pass through. `to` is one of
    /// `Timestamp` / `Timestamptz` / `Date`.
    DateConvert {
        inner: Box<RExpr>,
        to: ScalarType,
    },
    /// A clock-relative date literal — `'today'` / `'now'` (0), `'tomorrow'` (+1), `'yesterday'`
    /// (−1) — resolved to a STABLE node, never folded (date.md §6): at eval it reads the
    /// STATEMENT clock (once per statement, like `now()`) and takes its day in the SESSION zone
    /// (charging `timezone`), shifted by `offset_days`. Flagged non-immutable at birth (42P17 in
    /// an index expression). `'epoch'` is not this node — it folds to the constant 1970-01-01.
    DateClock {
        offset_days: i32,
    },
    /// A resolved `CASE` (grammar.md §23). `arms` is `(condition, result)` pairs — the condition
    /// is the searched boolean predicate, or the simple form's resolved `operand = value`
    /// equality. `els` is the ELSE result (`ConstNull` for an implicit ELSE). Evaluated lazily:
    /// the first TRUE condition's result wins. `coerce_decimal` is set when the unified result
    /// type is decimal, so integer results widen to decimal at eval.
    Case {
        arms: Vec<(RExpr, RExpr)>,
        els: Box<RExpr>,
        coerce_decimal: bool,
    },
    /// A resolved `COALESCE(a, b, …)` (grammar.md §51) — lazy like `Case`: arguments are
    /// evaluated left to right, each at most once, stopping at the first non-NULL (the second
    /// sanctioned short-circuit, cost.md §3). Argument types unify exactly like CASE result
    /// arms; `coerce_decimal` widens integer arguments when the unified type is decimal.
    Coalesce {
        args: Vec<RExpr>,
        coerce_decimal: bool,
    },
    /// A resolved `GREATEST(a, b, …)` / `LEAST(a, b, …)` (grammar.md §52) — the variadic max/min.
    /// EAGER (unlike `Coalesce`): every argument is evaluated. NULL arguments are ignored; the
    /// result is NULL only when every argument is NULL. `greatest` selects max vs min; the winner
    /// is chosen by the unified type's total order (`value_cmp`, or `collation` for text).
    /// Argument types unify to one common ORDERABLE type (grammar.md §52 — numeric promote, float
    /// widths widen to f64, other families structural; a non-orderable type such as json/jsonpath
    /// is rejected at resolve). `coerce_decimal` widens integer arguments when the unified type is
    /// decimal; `collation` is the derived text comparison collation (None ⇒ byte order).
    GreatestLeast {
        args: Vec<RExpr>,
        coerce_decimal: bool,
        greatest: bool,
        collation: Option<std::sync::Arc<Collation>>,
    },
    /// A resolved `NULLIF(a, b)` (grammar.md §58) — `args` is exactly `[a, b]`. EAGER: both
    /// arguments are evaluated, `a` first. When both are non-NULL and `a = b` the result is NULL;
    /// otherwise it is `a`. The pair passed the `=` comparability gate at resolve, and the result
    /// type is `a`'s, except that an integer `a` facing a decimal `b` becomes decimal
    /// (`coerce_decimal`, PostgreSQL's `int = numeric` resolution). `widen_float` widens an f32
    /// operand to f64 for the comparison only (a mixed-width float pair; the result keeps `a`'s
    /// width, PostgreSQL's `float48eq`). Equality is byte equality under every deterministic
    /// collation, so no collation is carried (collation.md §7).
    NullIf {
        args: Vec<RExpr>,
        coerce_decimal: bool,
        widen_float: bool,
    },
    /// A scalar-function call (abs/round, spec/design/functions.md §9), evaluated per row in
    /// any context. `result` is the static result type — for `abs` over an integer it is the
    /// operand's integer type, so the magnitude is range-checked at that boundary (the same
    /// 22003 discipline as `Neg`); for `abs` over decimal and all `round` forms it is decimal.
    /// Arguments propagate NULL.
    ScalarFunc {
        func: ScalarFunc,
        args: Vec<RExpr>,
        result: ScalarType,
    },
    /// A **host-defined** scalar-function call (spec/design/extensibility.md §4.2 / §5.1) — the
    /// injection seam that reaches a host-registered kernel a compiled `match` on the closed
    /// [`ScalarFunc`] enum cannot. `id` indexes the host functions of the session's frozen
    /// [`Environment`](crate::Environment); `result` is the host's declared scalar result type. STRICT (a NULL arg → NULL,
    /// short-circuited in eval like [`RExpr::ScalarFunc`]). Kept a **separate** variant so the
    /// built-in path stays a monomorphized inline `match` (§5) and no built-in id-reasoning applies
    /// to host code.
    HostFunc {
        id: usize,
        /// The (exact) function name, carried on the node so EXPLAIN can render it without the
        /// registry (which the `&RExpr`-only printer does not have). Redundant with the registry
        /// entry at `id`; a few bytes per node.
        name: String,
        args: Vec<RExpr>,
        result: ScalarType,
    },
    /// A polymorphic array-function call (spec/design/array-functions.md §3). Resolved over the
    /// `anyarray`/`anyelement` pseudo-families (§2); the resolved element/array type lives in the
    /// surrounding `ResolvedType` (carried out of resolve), not on the node — the kernel produces
    /// the result `Value` from the operand values alone (an array `Value` is self-describing). The
    /// introspectors propagate NULL; the builders (`array_append`/`prepend`/`cat`) are non-strict
    /// (`null = "none"`), so NULL handling lives in the kernel, not a blanket short-circuit here.
    ArrayFunc {
        func: ArrayFunc,
        args: Vec<RExpr>,
    },
    /// A polymorphic range accessor (spec/design/range-functions.md §1 — lower/upper/isempty/
    /// lower_inc/upper_inc/lower_inf/upper_inf). Like [`RExpr::ArrayFunc`], the resolved element type
    /// lives in the surrounding `ResolvedType`; the kernel produces the result `Value` from the
    /// operand range value alone (self-describing). All are STRICT (NULL handled in the kernel).
    RangeFunc {
        func: RangeFunc,
        args: Vec<RExpr>,
    },
    /// A regular-expression scalar function (spec/design/regex.md §8 — `regexp_replace` → text,
    /// `regexp_match` → text[]). Like [`RExpr::ArrayFunc`] the result type lives in the surrounding
    /// `ResolvedType` (text or text[]), not a scalar `result`. STRICT (a NULL arg → NULL, short-
    /// circuited in eval). `args` are the resolved text operands (source, pattern, [replacement,]
    /// [flags]); `program` is the precompiled NFA for a constant pattern (regex.md §5), `compile_charged`
    /// the one-shot flag charging its `regex_compile` cost once per execution.
    RegexFunc {
        func: RegexFunc,
        args: Vec<RExpr>,
        program: Option<Box<crate::regex::Program>>,
        /// The precompiled matcher of a `SimilarSubstring` with a constant pattern and escape.
        similar: Option<Box<crate::regex::SimilarMatcher>>,
        compile_charged: std::cell::Cell<bool>,
    },
    /// A range CONSTRUCTOR call (spec/design/range-functions.md §2 — `i32range(lo, hi[, bounds])` and
    /// the five siblings). `elem` is the range's element scalar (the result range type is recovered
    /// from it, a bijection); `args` are the 2 bounds plus an optional bounds-flags TEXT. Non-strict
    /// (`null = "none"`): a NULL bound is an infinite bound, handled in the kernel. The kernel coerces
    /// each bound to `elem` (assignment-style), reads the bounds flags, and finalizes (canonicalize /
    /// order-check / empty-normalize).
    RangeCtor {
        elem: ScalarType,
        args: Vec<RExpr>,
    },
    /// A range BOOLEAN operator (spec/design/range-functions.md §3 — `@> <@ && << >> &< &> -|-`).
    /// `args` are the two operands. STRICT: a NULL operand → NULL (handled in the eval arm). `elem`
    /// is the range's element scalar — used only by the `ContainsElem`/`ElemContainedBy` element
    /// overloads to coerce the bare-element operand to the range's element type at eval; unused (but
    /// carried) for the range-against-range operators.
    RangeOp {
        op: RangeOp,
        args: Vec<RExpr>,
        elem: ScalarType,
    },
    /// A range SET operator (spec/design/range-functions.md §4 — `+` union, `-` difference, `*`
    /// intersection, and `range_merge`). `args` are the two range operands. STRICT: a NULL operand →
    /// NULL (handled in the eval arm). Unlike [`RExpr::RangeOp`] it carries no element scalar — the
    /// kernels work off the self-describing operand values, and the result range type is fixed at
    /// resolve. The kernels (`range_union`/`range_intersect`/`range_minus`) live in `range.rs`;
    /// `+`/`-` raise 22000 on a non-contiguous result.
    RangeSetOp {
        op: RangeSetOp,
        args: Vec<RExpr>,
    },
    /// A VARIADIC argument-counting call (spec/design/array-functions.md §12 — num_nulls/
    /// num_nonnulls). Non-strict (`null = "none"`): the kernel inspects null-ness itself, so there
    /// is no blanket NULL short-circuit. `array_form` records the call shape: `false` = the SPREAD
    /// form (count `args`' null-ness directly, never NULL); `true` = the VARIADIC-array form (one
    /// `args` operand — a NULL array → NULL, else count its flattened elements' null-ness). Result
    /// is always i32.
    Variadic {
        func: VariadicFunc,
        args: Vec<RExpr>,
        array_form: bool,
    },
    /// A VARIADIC json/jsonb builder (json-sql-functions.md §2 — json[b]_build_array / _object).
    /// Non-strict: a NULL argument is included as JSON null (array) or a value (object). `json`
    /// selects the json (compact / PG builder-spacing) vs jsonb (canonical) render; `array_form`
    /// records the VARIADIC-array call shape (the lone array operand is spread; a NULL array → NULL).
    JsonBuild {
        kind: JsonBuildKind,
        json: bool,
        args: Vec<RExpr>,
        array_form: bool,
        /// Each argument's static type (the VARIADIC-array form: the lone array's type), which keys
        /// a composite argument's members.
        tys: Vec<ResolvedType>,
    },
    /// A JSON-image function — `to_json`, `to_jsonb`, `array_to_json`, `row_to_json`
    /// (json-sql-functions.md §2.2): `args` are the value and, for the array/row forms, the optional
    /// boolean `pretty` flag; `ty` is the value's static type (a composite's field names key its
    /// members), or `Null` for an untyped NULL argument. STRICT (a NULL operand → NULL).
    JsonImage {
        func: JsonImageFunc,
        args: Vec<RExpr>,
        ty: ResolvedType,
    },
    /// A correlated column reference (spec/design/grammar.md §26): the column `index` of the
    /// enclosing-query row `level` hops out (1 = immediate parent). A **leaf** — charges no
    /// `operator_eval`, like `Column`; at eval it reads from the outer-row environment.
    OuterColumn {
        level: usize,
        index: usize,
    },
    /// A subquery resolved once against the scope chain (spec/design/grammar.md §26). After the
    /// `fold_uncorrelated` pass only CORRELATED subqueries survive as this node (uncorrelated
    /// ones are folded to a constant / `InValues`). At eval the inner `plan` is re-executed
    /// against the pushed outer-row environment, once per outer row that reaches this node.
    /// (The element/scalar static type was settled at resolve — the value alone suffices here.)
    Subquery {
        plan: Box<QueryPlan>,
        kind: SubqueryKind,
        /// For `In`: the outer value tested for membership. `None` for Scalar/Exists.
        lhs: Option<Box<RExpr>>,
        /// For `In` / `Exists`: the `NOT` flag.
        negated: bool,
        /// A ROW-VALUED subquery (grammar.md §26 "Row-valued subqueries"): the left operand is a
        /// row constructor, so each body row packages its N columns as one anonymous composite
        /// value (`Value::Composite`) instead of yielding its single column. Never set for Exists.
        row_valued: bool,
        /// For a point-capable `In` / `= ANY` membership subquery: the cell the fold fills with its
        /// value list, shared with any interval-set bound planned over it (cost.md §3 "OR / IN-list").
        points: Option<std::sync::Arc<FoldedPoints>>,
    },
    /// A folded **uncorrelated** `IN (subquery)` (spec/design/grammar.md §26): the subquery ran
    /// once (at the fold pass) yielding the constant `list`; per row it tests `lhs` for 3-valued
    /// membership (empty → `negated`; a NULL with no positive match → NULL). One `operator_eval`
    /// for the node plus one per element compared.
    InValues {
        lhs: Box<RExpr>,
        list: Vec<Value>,
        negated: bool,
        /// The folded subquery's point flags, when its values are points of `lhs`'s type.
        points: Option<std::sync::Arc<FoldedPoints>>,
    },
    /// A quantified array comparison `lhs op ANY/ALL(array)` (spec/design/array-functions.md §11):
    /// the array spelling of `IN`. At eval `lhs` is evaluated ONCE, `array` once; then a 3-valued
    /// fold over the array's flattened elements — `ANY` (all=false) is the OR-fold (TRUE if any
    /// `lhs op e` is TRUE, else NULL if any is NULL, else FALSE; empty → FALSE), `ALL` (all=true)
    /// the AND-fold (FALSE if any is FALSE, else NULL if any is NULL, else TRUE; empty → TRUE). A
    /// NULL array → NULL. Charges like `InValues`: one `operator_eval` for the node plus one per
    /// element compared (so `max_cost` bounds the walk).
    Quantified {
        op: CmpOp,
        all: bool,
        lhs: Box<RExpr>,
        array: Box<RExpr>,
        /// The folded form of a ROW-VALUED quantified subquery (grammar.md §26): each element is
        /// compared by the row-constructor 3VL (`eq3`/`lt3`/`gt3`, like `RExpr::Compare`), not the
        /// composite total order the array form uses for composite elements (array-functions.md §13).
        row_wise: bool,
        /// The array's element type is exactly the scalar type of `lhs` (no promotion), so each
        /// element is a point in `lhs`'s key space — what lets `key = ANY(array)` bound a key scan
        /// (cost.md §3 "OR / IN-list").
        elem_exact: bool,
        /// The element type is not the scalar type of `lhs`, but every element value either
        /// converts exactly to it or provably equals none of its values — an integer of another
        /// width, or an integer against a decimal in either direction — so a value-readable array
        /// still yields points in `lhs`'s key space after conversion ([`point_value`]; cost.md §3
        /// "OR / IN-list").
        ///
        /// [`point_value`]: crate::executor::access_encode::point_value
        elem_promoted: bool,
        /// The comparison collation (spec/design/collation.md §1): an ordering quantifier over text
        /// sorts by it. The left operand's derivation for the array form (an array carries none), the
        /// subquery form's combined one when folded from it; `None` for `C` / a non-text pair.
        collation: Option<std::sync::Arc<Collation>>,
    },
    /// `width_bucket(operand, thresholds)` — the array-threshold variant (array-functions.md §14),
    /// PostgreSQL's `width_bucket_array`: a binary search over a thresholds array ASSUMED sorted
    /// ascending (not checked), yielding the count of thresholds `<=` operand (the 1-based bucket,
    /// 0 below the first) as `i32`. STRICT like a scalar function: a NULL operand → NULL without
    /// evaluating `thresholds`, a NULL array → NULL. A multidimensional array is 2202E, a NULL
    /// element 22004. Charges one `operator_eval` for the node plus one per comparison step the
    /// search makes (plus `decimal_work` / `collate` like the quantified walk).
    WidthBucketArray {
        operand: Box<RExpr>,
        thresholds: Box<RExpr>,
        /// The comparison collation for a text operand/thresholds pair (collation.md §1): the
        /// combined derivation of both arguments, `None` for `C` / a non-text pair.
        collation: Option<std::sync::Arc<Collation>>,
    },
}

/// Which subquery form an `RExpr::Subquery` is (spec/design/grammar.md §26).
#[derive(Clone)]
pub(crate) enum SubqueryKind {
    Scalar,
    Exists,
    In,
    /// `lhs op ANY/ALL(SELECT …)` — the quantified-subquery form (array-functions.md §11.6). `lhs`
    /// is the outer value; the body's single column folds through `quantified_membership` exactly
    /// like the array `Quantified` node. Survives as a `Subquery` node only when CORRELATED; an
    /// uncorrelated one is folded to a constant-array `RExpr::Quantified`.
    Quantified {
        op: CmpOp,
        all: bool,
        /// The comparison collation (spec/design/collation.md §1): the left operand combined with
        /// the body column's implicit collation; `None` for `C` / a non-text pair.
        collation: Option<std::sync::Arc<Collation>>,
    },
}
