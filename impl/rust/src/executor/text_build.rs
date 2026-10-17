//! The text-building VARIADIC "any" kernels (spec/design/string-functions.md §6; mirrors impl/go
//! text_build.go): `concat`, `concat_ws`, and `format`, over their already-evaluated argument values.

use super::*;

/// The hint PostgreSQL attaches to a malformed `format()` specifier (varlena.c).
const FORMAT_HINT: &str = "For a single \"%\" use \"%%\".";

/// Evaluate a `concat` / `concat_ws` / `format` node (the caller charged its `operator_eval`).
/// `args` are the fixed parameter (none for concat; the separator / format string) followed by the
/// spread arguments, or by the one array of the VARIADIC-array form. Non-strict: every argument is
/// evaluated, then a NULL separator / format string yields NULL. A NULL VARIADIC array yields NULL
/// for concat / concat_ws and is no arguments for format, as in PostgreSQL.
pub(crate) fn eval_text_variadic(
    func: &VariadicFunc,
    args: &[RExpr],
    array_form: bool,
    row: &[Value],
    env: &EvalEnv,
    m: &mut Meter,
) -> Result<Value> {
    let mut vals = Vec::with_capacity(args.len());
    for a in args {
        vals.push(a.eval(row, env, m)?);
    }
    let fixed = usize::from(!matches!(func, VariadicFunc::Concat));
    let head = match vals.first() {
        Some(Value::Null) if fixed == 1 => return Ok(Value::Null),
        Some(Value::Text(s)) if fixed == 1 => s.clone(),
        _ => String::new(),
    };
    let rest: Vec<Value> = if array_form {
        match &vals[fixed] {
            Value::Null if matches!(func, VariadicFunc::Format) => Vec::new(),
            Value::Null => return Ok(Value::Null),
            Value::Array(a) => a.elements.clone(),
            _ => unreachable!("resolver restricts a VARIADIC operand to an array"),
        }
    } else {
        vals.split_off(fixed)
    };
    let zone = &env.exec.session.time_zone;
    let out = match func {
        VariadicFunc::Concat => {
            let out = Value::Text(concat_values(&rest, "", false, zone, m)?);
            // A growth kernel (cost.md §8.1), free over constant operands.
            if !args.iter().all(rexpr_is_constant) {
                charge_output(m, &out)?;
            }
            out
        }
        VariadicFunc::ConcatWs => Value::Text(concat_values(&rest, &head, true, zone, m)?),
        VariadicFunc::Format => Value::Text(format_text(&head, &rest, zone, m)?),
        _ => unreachable!("eval_text_variadic: not a text-building function"),
    };
    Ok(out)
}

/// One argument's OUTPUT text (string-functions.md §6): the text PostgreSQL's type output function
/// produces. That is the value's rendering, except a boolean, which is `t`/`f` (boolout) rather than
/// the `true`/`false` of the `::text` cast. A finite `timestamptz` at any depth renders in the
/// session zone and charges one `timezone` unit, charged and guarded before rendering. The caller
/// handles NULL.
pub(crate) fn output_text(
    v: &Value,
    zone: &crate::timezone::ZoneRef,
    m: &mut Meter,
) -> Result<String> {
    match v {
        Value::Text(s) => Ok(s.clone()),
        Value::Bool(b) => Ok((if *b { "t" } else { "f" }).to_string()),
        _ => {
            let zoned = v.zone_render_count();
            if zoned > 0 {
                m.charge(COSTS.timezone * zoned as i64);
                m.guard()?;
            }
            v.render_in(zone)
        }
    }
}

/// `concat(VARIADIC "any")` / `concat_ws(sep, VARIADIC "any")` (string-functions.md §6): the
/// non-NULL values' output text joined by `sep` (`""` for concat). For `concat_ws` (`amplify`) the
/// kernel is an amplifier (cost.md §8.1): it sizes the result exactly, caps it at
/// `MAX_RESULT_CHARS` (`54000`), charges `scalar_byte` per output byte, guards, and reserves the
/// bytes before building. `concat` is a growth kernel and its caller charges the built result.
pub(crate) fn concat_values(
    vals: &[Value],
    sep: &str,
    amplify: bool,
    zone: &crate::timezone::ZoneRef,
    m: &mut Meter,
) -> Result<String> {
    let mut parts = Vec::with_capacity(vals.len());
    for v in vals {
        if !matches!(v, Value::Null) {
            parts.push(output_text(v, zone, m)?);
        }
    }
    if amplify {
        let mut size: i64 = parts.iter().map(|p| p.len() as i64).sum();
        if parts.len() > 1 {
            size += (parts.len() as i64 - 1) * sep.len() as i64;
        }
        if size > MAX_RESULT_CHARS {
            return Err(EngineError::new(
                SqlState::ProgramLimitExceeded,
                "requested length too large",
            ));
        }
        m.charge(COSTS.scalar_byte * size);
        m.guard()?;
        m.reserve_scalar(size)?;
    }
    Ok(parts.join(sep))
}

/// One piece of a `format()` result: a literal span of the format string, or a converted argument
/// with its padding (`pad` spaces, on the right when `left`).
enum FormatPiece {
    Lit(usize, usize),
    Arg {
        text: String,
        pad: usize,
        left: bool,
    },
}

fn format_unterminated() -> EngineError {
    EngineError::new(
        SqlState::InvalidParameterValue,
        "unterminated format() type specifier",
    )
    .hinted(FORMAT_HINT)
}

fn format_number_out_of_range() -> EngineError {
    EngineError::new(SqlState::NumericValueOutOfRange, "number is out of range")
}

fn format_too_few() -> EngineError {
    EngineError::new(
        SqlState::InvalidParameterValue,
        "too few arguments for format()",
    )
}

fn format_arg_zero() -> EngineError {
    EngineError::new(
        SqlState::InvalidParameterValue,
        "format specifies argument 0, but arguments are numbered from 1",
    )
}

/// Advance past byte `i` of the format string; reaching its end is the unterminated-specifier error
/// (PostgreSQL's ADVANCE_PARSE_POINTER).
fn format_advance(b: &[u8], i: usize) -> Result<usize> {
    if i + 1 >= b.len() {
        return Err(format_unterminated());
    }
    Ok(i + 1)
}

/// Parse a run of ASCII digits at `i` as a nonnegative i32 (PostgreSQL's
/// text_format_parse_digits): `Some(n)` when at least one digit was read, with the next position.
/// An i32 overflow is `22003`; digits that run to the end of the string are unterminated.
fn format_digits(b: &[u8], mut i: usize) -> Result<(Option<i32>, usize)> {
    let mut val: i32 = 0;
    let mut found = false;
    while b[i].is_ascii_digit() {
        val = val
            .checked_mul(10)
            .and_then(|v| v.checked_add(i32::from(b[i] - b'0')))
            .ok_or_else(format_number_out_of_range)?;
        i = format_advance(b, i)?;
        found = true;
    }
    Ok((found.then_some(val), i))
}

/// A parsed `%…` specifier, positioned at its conversion character (PostgreSQL's
/// text_format_parse_format). `argpos` and `widthpos` are 1-based; `widthpos == Some(0)` means a `*`
/// width taken from the next argument.
struct FormatSpec {
    argpos: Option<i32>,
    widthpos: Option<i32>,
    minus: bool,
    width: i32,
    at: usize,
}

fn format_parse_spec(b: &[u8], start: usize) -> Result<FormatSpec> {
    let mut spec = FormatSpec {
        argpos: None,
        widthpos: None,
        minus: false,
        width: 0,
        at: start,
    };
    let (n, mut i) = format_digits(b, start)?;
    if let Some(n) = n {
        if b[i] != b'$' {
            // Just a width and the conversion character.
            spec.width = n;
            spec.at = i;
            return Ok(spec);
        }
        if n == 0 {
            return Err(format_arg_zero());
        }
        spec.argpos = Some(n);
        i = format_advance(b, i)?;
    }
    while b[i] == b'-' {
        spec.minus = true;
        i = format_advance(b, i)?;
    }
    if b[i] == b'*' {
        i = format_advance(b, i)?;
        let (n, j) = format_digits(b, i)?;
        i = j;
        match n {
            Some(n) => {
                if b[i] != b'$' {
                    return Err(EngineError::new(
                        SqlState::InvalidParameterValue,
                        "width argument position must be ended by \"$\"",
                    ));
                }
                if n == 0 {
                    return Err(format_arg_zero());
                }
                spec.widthpos = Some(n);
                i = format_advance(b, i)?;
            }
            None => spec.widthpos = Some(0),
        }
    } else {
        let (n, j) = format_digits(b, i)?;
        i = j;
        if let Some(n) = n {
            spec.width = n;
        }
    }
    spec.at = i;
    Ok(spec)
}

/// A `*` width argument as an i32 (PostgreSQL reads an int2/int4 directly and converts any other type
/// through its output text). NULL is width 0; an integer outside i32 is `22003`; any other value goes
/// through the text → i32 cast, so a non-integer is that cast's `22P02` / `22003`.
fn format_width_value(v: &Value, zone: &crate::timezone::ZoneRef, m: &mut Meter) -> Result<i32> {
    match v {
        Value::Null => Ok(0),
        Value::Int(n) => i32::try_from(*n).map_err(|_| overflow(ScalarType::Int32)),
        _ => {
            let text = output_text(v, zone, m)?;
            Ok(parse_int_literal(&text, ScalarType::Int32)? as i32)
        }
    }
}

/// `format(formatstr, VARIADIC "any")` (string-functions.md §6), PostgreSQL's text_format over
/// `args` (the values after the format string). An amplifier (cost.md §8.1): the format string's
/// UTF-8 bytes are charged as the input scan and guarded; one pass parses it and renders each
/// converted argument (the specifier errors surface in scan order); the exact output size is then
/// capped at `MAX_RESULT_CHARS` (`54000`), charged, guarded, and reserved before the result is built.
pub(crate) fn format_text(
    fmt: &str,
    args: &[Value],
    zone: &crate::timezone::ZoneRef,
    m: &mut Meter,
) -> Result<String> {
    m.charge(COSTS.scalar_byte * fmt.len() as i64);
    m.guard()?;
    let b = fmt.as_bytes();
    let nargs = args.len() + 1; // the format string is argument 0
    let mut arg: usize = 1; // the next argument to print
    let mut pieces = Vec::new();
    let mut lit = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b'%' {
            i += 1;
            continue;
        }
        if lit < i {
            pieces.push(FormatPiece::Lit(lit, i));
        }
        i = format_advance(b, i)?;
        if b[i] == b'%' {
            pieces.push(FormatPiece::Lit(i, i + 1));
            i += 1;
            lit = i;
            continue;
        }
        let spec = format_parse_spec(b, i)?;
        i = spec.at;
        let conv = b[i];
        if !matches!(conv, b's' | b'I' | b'L') {
            let ch = fmt[i..]
                .chars()
                .next()
                .expect("a character at a char boundary");
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                format!("unrecognized format() type specifier \"{ch}\""),
            )
            .hinted(FORMAT_HINT));
        }
        let mut width = spec.width;
        if let Some(wp) = spec.widthpos {
            if wp > 0 {
                arg = wp as usize;
            }
            if arg >= nargs {
                return Err(format_too_few());
            }
            width = format_width_value(&args[arg - 1], zone, m)?;
            arg += 1;
        }
        if let Some(ap) = spec.argpos {
            arg = ap as usize;
        }
        if arg >= nargs {
            return Err(format_too_few());
        }
        let v = &args[arg - 1];
        arg += 1;
        let text = match (v, conv) {
            (Value::Null, b's') => String::new(),
            (Value::Null, b'L') => "NULL".to_string(),
            (Value::Null, _) => {
                return Err(EngineError::new(
                    SqlState::NullValueNotAllowed,
                    "null values cannot be formatted as an SQL identifier",
                ));
            }
            (v, b'I') => quote_ident_text(&output_text(v, zone, m)?),
            (v, b'L') => quote_literal_text(&output_text(v, zone, m)?),
            (v, _) => output_text(v, zone, m)?,
        };
        let mut left = spec.minus;
        if width < 0 {
            // A negative width is an implicit `-` flag; -INT_MIN is unrepresentable.
            if width == i32::MIN {
                return Err(format_number_out_of_range());
            }
            left = true;
            width = -width;
        }
        let chars = text.chars().count();
        let pad = (width as usize).saturating_sub(chars);
        pieces.push(FormatPiece::Arg { text, pad, left });
        i += 1;
        lit = i;
    }
    if lit < b.len() {
        pieces.push(FormatPiece::Lit(lit, b.len()));
    }
    let size: i64 = pieces
        .iter()
        .map(|p| match p {
            FormatPiece::Lit(s, e) => (e - s) as i64,
            FormatPiece::Arg { text, pad, .. } => text.len() as i64 + *pad as i64,
        })
        .sum();
    if size > MAX_RESULT_CHARS {
        return Err(EngineError::new(
            SqlState::ProgramLimitExceeded,
            "requested length too large",
        ));
    }
    m.charge(COSTS.scalar_byte * size);
    m.guard()?;
    m.reserve_scalar(size)?;
    let mut out = String::with_capacity(size as usize);
    for p in &pieces {
        match p {
            FormatPiece::Lit(s, e) => out.push_str(&fmt[*s..*e]),
            FormatPiece::Arg { text, pad, left } => {
                if *left {
                    out.push_str(text);
                    out.extend(std::iter::repeat_n(' ', *pad));
                } else {
                    out.extend(std::iter::repeat_n(' ', *pad));
                    out.push_str(text);
                }
            }
        }
    }
    Ok(out)
}
