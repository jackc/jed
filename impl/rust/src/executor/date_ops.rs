//! Date/timestamp conversion and arithmetic kernels: the casts between `date`, `timestamp` and
//! `timestamptz`, the clock-derived date values, and `date ± integer` / `date - date`
//! (date.md, timestamp.md).

use super::*;

/// Evaluate a cross-family datetime cast (timezones.md §9.3) — or a runtime text → datetime cast
/// (date.md §6, types.md §5) — of the non-NULL value `v` to `to` (`Timestamp`/`Timestamptz`/`Date`).
/// The casts crossing the `timestamptz` boundary consult the session zone (charging `timezone`);
/// the others are zone-free. `±infinity` maps to the target's own sentinel. The `(source, to)` pair
/// is guaranteed by the resolver: cross-family datetime, or text → datetime.
pub(crate) fn eval_date_convert(
    v: Value,
    to: ScalarType,
    env: &EvalEnv,
    m: &mut Meter,
) -> Result<Value> {
    use crate::timestamp::{NEG_INFINITY as TS_NEG, POS_INFINITY as TS_POS};
    const MICROS_PER_DAY: i64 = 86_400 * 1_000_000;
    // Map a finite/infinite timestamp-micros to a date (days), preserving the ±inf sentinels.
    let micros_to_date = |mc: i64| -> Value {
        if mc == TS_POS {
            Value::Date(crate::date::POS_INFINITY)
        } else if mc == TS_NEG {
            Value::Date(crate::date::NEG_INFINITY)
        } else {
            Value::Date(mc.div_euclid(MICROS_PER_DAY) as i32)
        }
    };
    let is_inf = |mc: i64| mc == TS_POS || mc == TS_NEG;
    match (v, to) {
        // text -> date (the runtime text → date cast, date.md §6): the per-row string runs the
        // SAME parse_date the literal form folds at resolve — 22007 malformed / 22008 out of
        // range, per row. Zone-free (the node's operator_eval meters it) — EXCEPT a
        // clock-relative special ('today'/'now'/…), which names the statement-clock day in the
        // session zone (a zone consult, charging timezone) — the reason this cast is STABLE.
        (Value::Text(s), ScalarType::Date) => {
            if let Some((offset_days, epoch)) = crate::date::date_clock_special(&s) {
                if epoch {
                    return Ok(Value::Date(0));
                }
                return date_clock_value(env.exec, env.rng, m, offset_days);
            }
            Ok(Value::Date(crate::date::parse_date(&s)?))
        }
        // text -> timestamp (types.md §5): the literal's parse_timestamp per row — an offset is
        // validated and ignored; 22007 malformed / 22008 out of range. Zone-free.
        (Value::Text(s), ScalarType::Timestamp) => {
            Ok(Value::Timestamp(crate::timestamp::parse_timestamp(&s)?))
        }
        // text -> timestamptz (types.md §5): an explicit offset (or ±infinity) fixes the instant;
        // an offset-less wall clock is read in the SESSION zone, as PostgreSQL's timestamptz_in
        // does (charging timezone; DST gap/overlap as the timestamp -> timestamptz cast).
        (Value::Text(s), ScalarType::Timestamptz) => {
            use crate::timestamp::TimestamptzInput;
            match crate::timestamp::parse_timestamptz_input(&s)? {
                TimestamptzInput::Instant(mc) => Ok(Value::Timestamptz(mc)),
                TimestamptzInput::Local(wall) => {
                    let zr = env.exec.session.time_zone.clone();
                    m.charge(COSTS.timezone);
                    m.guard()?;
                    let mc = crate::timezone::local_to_instant_micros(&zr, wall)
                        .ok_or_else(crate::timestamp::zone_shift_out_of_range)?;
                    Ok(Value::Timestamptz(mc))
                }
            }
        }
        // timestamp -> date (zone-free): the date part.
        (Value::Timestamp(mc), ScalarType::Date) => Ok(micros_to_date(mc)),
        // date -> timestamp (zone-free): midnight; a date past the timestamp range traps 22008.
        (Value::Date(d), ScalarType::Timestamp) => Ok(Value::Timestamp(date_midnight_micros(d)?)),
        // timestamptz -> timestamp: render the instant in the session zone.
        (Value::Timestamptz(mc), ScalarType::Timestamp) => {
            if is_inf(mc) {
                return Ok(Value::Timestamp(mc));
            }
            let zr = env.exec.session.time_zone.clone();
            m.charge(COSTS.timezone);
            m.guard()?;
            let local = crate::timezone::instant_to_local_micros(&zr, mc)
                .ok_or_else(crate::timestamp::zone_shift_out_of_range)?;
            Ok(Value::Timestamp(local))
        }
        // timestamp -> timestamptz: interpret the wall clock in the session zone.
        (Value::Timestamp(mc), ScalarType::Timestamptz) => {
            if is_inf(mc) {
                return Ok(Value::Timestamptz(mc));
            }
            let zr = env.exec.session.time_zone.clone();
            m.charge(COSTS.timezone);
            m.guard()?;
            let instant = crate::timezone::local_to_instant_micros(&zr, mc)
                .ok_or_else(crate::timestamp::zone_shift_out_of_range)?;
            Ok(Value::Timestamptz(instant))
        }
        // timestamptz -> date: the date of the session-zone wall clock.
        (Value::Timestamptz(mc), ScalarType::Date) => {
            if is_inf(mc) {
                return Ok(micros_to_date(mc));
            }
            let zr = env.exec.session.time_zone.clone();
            m.charge(COSTS.timezone);
            m.guard()?;
            let local = crate::timezone::instant_to_local_micros(&zr, mc)
                .ok_or_else(crate::timestamp::zone_shift_out_of_range)?;
            Ok(micros_to_date(local))
        }
        // date -> timestamptz: midnight in the session zone -> the instant.
        (Value::Date(d), ScalarType::Timestamptz) => {
            let mid = date_midnight_micros(d)?;
            if is_inf(mid) {
                return Ok(Value::Timestamptz(mid));
            }
            let zr = env.exec.session.time_zone.clone();
            m.charge(COSTS.timezone);
            m.guard()?;
            let instant = crate::timezone::local_to_instant_micros(&zr, mid)
                .ok_or_else(crate::timestamp::zone_shift_out_of_range)?;
            Ok(Value::Timestamptz(instant))
        }
        _ => unreachable!(
            "resolver restricts DateConvert to cross-family datetime casts and text → datetime"
        ),
    }
}

/// Compute a clock-relative date (date.md §6): the STATEMENT clock's day in the SESSION zone,
/// shifted by `offset_days` (`'today'`/`'now'` 0, `'tomorrow'` +1, `'yesterday'` −1). STABLE —
/// the statement clock is read once per statement and reused (entropy.md §5), so every row and
/// every clock-relative literal in a statement see the same day. The zone consultation charges
/// one `timezone` unit, like every session-zone consumer (timezones.md §10); the caller's node
/// charges its own `operator_eval`. Shared by the `DateClock` eval, the runtime text→date cast's
/// special path, and the INSERT VALUES literal-adaptation site (env-less, hence the split args).
pub(crate) fn date_clock_value(
    exec: &Engine,
    rng: &std::cell::Cell<crate::seam::StmtRng>,
    m: &mut Meter,
    offset_days: i32,
) -> Result<Value> {
    const MICROS_PER_DAY: i64 = 86_400 * 1_000_000;
    let zr = exec.session.time_zone.clone();
    m.charge(COSTS.timezone);
    m.guard()?;
    let mut r = rng.get();
    let clock = r.statement_clock_micros(&exec.session.env);
    rng.set(r);
    // An injected extreme clock can shift past the timestamp range: 22008, like the day bound.
    let local = crate::timezone::instant_to_local_micros(&zr, clock)
        .ok_or_else(crate::timestamp::zone_shift_out_of_range)?;
    let days = local.div_euclid(MICROS_PER_DAY) + offset_days as i64;
    if !((i32::MIN as i64 + 1)..=(i32::MAX as i64 - 1)).contains(&days) {
        // Unreachable from a real clock; an injected extreme clock still traps deterministically.
        return Err(EngineError::new(
            SqlState::DatetimeFieldOverflow,
            "date out of range",
        ));
    }
    Ok(Value::Date(days as i32))
}

/// Midnight (00:00:00) of a `date` as timestamp microseconds, preserving the ±infinity sentinels.
/// A finite date whose midnight instant overflows the i64-µs timestamp range traps `22008` (jed's
/// date range is wider than the timestamp range — date.md §1). A finite day count cannot land on a
/// timestamp sentinel (`i64::MIN`/`MAX` are not multiples of a day's micros), so no sentinel-
/// collision check is needed here; `ts_shift` re-checks the shifted result anyway.
pub(crate) fn date_midnight_micros(d: i32) -> Result<i64> {
    use crate::timestamp::{NEG_INFINITY as TS_NEG, POS_INFINITY as TS_POS};
    const MICROS_PER_DAY: i64 = 86_400 * 1_000_000;
    if d == crate::date::POS_INFINITY {
        return Ok(TS_POS);
    }
    if d == crate::date::NEG_INFINITY {
        return Ok(TS_NEG);
    }
    i64::from(d)
        .checked_mul(MICROS_PER_DAY)
        .ok_or_else(|| EngineError::new(SqlState::DatetimeFieldOverflow, "date out of range"))
}

/// Evaluate a `date` arithmetic node (spec/design/date.md §6): `date ± int → date` (shift the i32
/// day count; ±infinity is returned unchanged; a finite result beyond the i32 day range or onto a
/// reserved sentinel traps `22008`), `date − date → i32` (days between; an ±infinity operand traps
/// `22008`, "cannot subtract infinite dates"; a difference beyond i32 traps `22008`), and
/// `date ± interval → timestamp` (the date widens to midnight, then the timestamp ± interval
/// calendar shift). The resolver guarantees a Date operand is present and settled `result`.
pub(crate) fn eval_date_arith(
    op: ArithOp,
    a: Value,
    b: Value,
    result: ScalarType,
) -> Result<Value> {
    use crate::date::{NEG_INFINITY as D_NEG, POS_INFINITY as D_POS};
    let dt_oflow = |msg: &'static str| EngineError::new(SqlState::DatetimeFieldOverflow, msg);

    // date ± interval → timestamp: widen the date to midnight micros, then the calendar shift.
    if matches!(result, ScalarType::Timestamp) {
        let (d, iv) = match (a, b) {
            (Value::Date(d), Value::Interval(iv)) | (Value::Interval(iv), Value::Date(d)) => {
                (d, iv)
            }
            _ => unreachable!("resolver guarantees a date ± interval pair"),
        };
        let mid = date_midnight_micros(d)?;
        let r = crate::interval::ts_shift(mid, &iv, matches!(op, ArithOp::Sub))?;
        return Ok(Value::Timestamp(r));
    }

    // date − date → i32 (days between); an ±infinity operand traps 22008.
    if let (Value::Date(x), Value::Date(y)) = (&a, &b) {
        if *x == D_NEG || *x == D_POS || *y == D_NEG || *y == D_POS {
            return Err(dt_oflow("cannot subtract infinite dates"));
        }
        let diff = i64::from(*x) - i64::from(*y);
        let diff = i32::try_from(diff).map_err(|_| dt_oflow("date out of range"))?;
        return Ok(Value::Int(i64::from(diff)));
    }

    // date ± int → date: shift the day count; a ±infinity date stays the same sentinel.
    let (d, n) = match (a, b) {
        (Value::Date(d), Value::Int(n)) | (Value::Int(n), Value::Date(d)) => (d, n),
        _ => unreachable!("resolver guarantees a date ± int pair"),
    };
    if d == D_NEG || d == D_POS {
        return Ok(Value::Date(d));
    }
    let shifted = if matches!(op, ArithOp::Sub) {
        i64::from(d).checked_sub(n)
    } else {
        i64::from(d).checked_add(n)
    }
    .ok_or_else(|| dt_oflow("date out of range"))?;
    let days = i32::try_from(shifted).map_err(|_| dt_oflow("date out of range"))?;
    // A finite result that lands on a reserved sentinel value is out of range.
    if days == D_NEG || days == D_POS {
        return Err(dt_oflow("date out of range"));
    }
    Ok(Value::Date(days))
}
