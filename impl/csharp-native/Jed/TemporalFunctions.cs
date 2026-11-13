using System.Globalization;

namespace Jed;

internal static class TemporalFunctions
{
    internal static bool TryCall(string name, SqlValue[] args, Meter meter, out SqlValue result)
    {
        result = default;
        switch (name)
        {
            case "now": result = SqlValue.Timestamp(meter.Temporal.StatementMicros, true); return true;
            case "clock_timestamp": result = SqlValue.Timestamp(meter.Temporal.ClockMicros(), true); return true;
            case "current_date":
                meter.Charge(Costs.Timezone); meter.Guard();
                result = SqlValue.Date(TemporalValue.TimestampToDate(meter.Temporal.ResolveZone().ToLocal(meter.Temporal.StatementMicros))); return true;
            case "make_date":
                var year = args[0].AsInteger(); var month = args[1].AsInteger(); var day = args[2].AsInteger();
                if (year == 0 || year < int.MinValue || year > int.MaxValue) throw TemporalValue.Overflow();
                TemporalValue.ValidateCivil(year < 0 ? year + 1 : year, month, day);
                result = SqlValue.Date(TemporalValue.MakeDate((int)year, (int)month, (int)day)); return true;
            case "make_interval":
                var seconds = RoundedMicros(args[6].AsFloat());
                try
                {
                    var months = checked(args[0].AsInteger() * 12 + args[1].AsInteger());
                    var days = checked(args[2].AsInteger() * 7 + args[3].AsInteger());
                    var time = checked(((args[4].AsInteger() * 60 + args[5].AsInteger()) * 60) * TemporalValue.MicrosPerSecond + seconds);
                    result = SqlValue.Interval(SqlInterval.Create(months, days, time)); return true;
                }
                catch (OverflowException) { throw TemporalValue.Overflow(); }
            case "make_timestamp": case "make_timestamptz":
                var wall = MakeTimestamp(args[0].AsInteger(), args[1].AsInteger(), args[2].AsInteger(), args[3].AsInteger(), args[4].AsInteger(), args[5].AsFloat());
                if (name == "make_timestamptz")
                {
                    meter.Charge(Costs.Timezone); meter.Guard();
                    if (args.Length == 7 && args[6].AsText().Length > 0 && args[6].AsText()[0] is '+' or '-')
                        wall = TemporalValue.FiniteTimestamp((Int128)wall - (long)MakeOffset(args[6].AsText()) * TemporalValue.MicrosPerSecond);
                    else wall = meter.Temporal.ResolveZone(args.Length == 7 ? args[6].AsText() : null).ToInstant(wall);
                }
                result = SqlValue.Timestamp(wall, name == "make_timestamptz"); return true;
            case "justify_hours": result = SqlValue.Interval(args[0].AsInterval().JustifyHours()); return true;
            case "justify_days": result = SqlValue.Interval(args[0].AsInterval().JustifyDays()); return true;
            case "justify_interval": result = SqlValue.Interval(args[0].AsInterval().Justify()); return true;
            case "age":
                var first = args.Length == 2 ? args[0].AsTimestamp() : TemporalValue.DateToTimestamp(TemporalValue.TimestampToDate(meter.Temporal.ResolveZone().ToLocal(meter.Temporal.StatementMicros)));
                var second = args[^1].AsTimestamp();
                if (TemporalValue.IsInfinite(first) || TemporalValue.IsInfinite(second)) throw TemporalValue.Overflow();
                if (args.Length == 1) { meter.Charge(Costs.Timezone); meter.Guard(); }
                if (args[0].Type == SqlType.Timestamptz) { meter.Charge(2 * Costs.Timezone); meter.Guard(); }
                if (args[0].Type == SqlType.Timestamptz)
                {
                    var zone = meter.Temporal.ResolveZone();
                    if (args.Length == 1) { meter.Charge(Costs.Timezone); meter.Guard(); first = zone.ToInstant(first); }
                    result = SqlValue.Interval(IntervalMath.Age(first, second, zone.ToLocal(first), zone.ToLocal(second)));
                }
                else result = SqlValue.Interval(IntervalMath.Age(first, second));
                return true;
            case "to_timestamp" when args.Length == 1 && args[0].Type.IsFloat():
                result = SqlValue.Timestamp(EpochTimestamp(args[0].AsFloat()), true); return true;
            case "date_part": result = Extract(args[0].AsText(), args[1], meter, asFloat: true); return true;
            case "date_trunc": result = Truncate(args[0].AsText(), args[1], meter, args.Length == 3 ? args[2].AsText() : null); return true;
            default: return false;
        }
    }
    internal static SqlValue AtTimeZone(SqlValue value, SqlValue name, Meter meter)
    {
        var target = value.Type == SqlType.Timestamptz ? SqlType.Timestamp : SqlType.Timestamptz;
        if (value.IsNull || name.IsNull) return SqlValue.Null(target);
        if (TemporalValue.IsInfinite(value.AsTimestamp())) return SqlValue.Timestamp(value.AsTimestamp(), target == SqlType.Timestamptz);
        var zone = meter.Temporal.ResolveZone(name.AsText());
        meter.Charge(Costs.Timezone); meter.Guard();
        return SqlValue.Timestamp(value.Type == SqlType.Timestamptz ? zone.ToLocal(value.AsTimestamp()) : zone.ToInstant(value.AsTimestamp()), target == SqlType.Timestamptz);
    }
    internal static long MakeTimestamp(long year, long month, long day, long hour, long minute, double second)
    {
        if (year == 0 || year < -999999 || year > 999999) throw TemporalValue.Overflow();
        var astronomical = year < 0 ? year + 1 : year;
        TemporalValue.ValidateCivil(astronomical, month, day);
        if (hour < 0 || hour > 24 || minute < 0 || minute > 59) throw TemporalValue.Overflow();
        var secondsMicros = RoundedMicros(second);
        if (secondsMicros < 0 || secondsMicros > 60_000_000) throw TemporalValue.Overflow();
        var time = (hour * 3600 + minute * 60) * TemporalValue.MicrosPerSecond + secondsMicros;
        if (time > TemporalValue.MicrosPerDay) throw TemporalValue.Overflow();
        return TemporalValue.FiniteTimestamp((Int128)TemporalValue.DaysFromCivil(astronomical, (int)month, (int)day) * TemporalValue.MicrosPerDay + time);
    }
    private static long RoundedMicros(double seconds)
    {
        var rounded = Math.Round(seconds * TemporalValue.MicrosPerSecond, MidpointRounding.AwayFromZero);
        if (!double.IsFinite(rounded) || rounded < -9223372036854775808d || rounded >= 9223372036854775808d) throw TemporalValue.Overflow();
        return (long)rounded;
    }
    private static long EpochTimestamp(double seconds)
    {
        if (double.IsNaN(seconds)) throw TemporalValue.Overflow();
        if (double.IsInfinity(seconds)) return seconds < 0 ? long.MinValue : long.MaxValue;
        var rounded = Math.Round((seconds - 946_684_800d) * TemporalValue.MicrosPerSecond, MidpointRounding.ToEven);
        if (rounded < -9223372036854775808d || rounded >= 9223372036854775808d) throw TemporalValue.Overflow();
        return TemporalValue.FiniteTimestamp((Int128)(long)rounded + TemporalValue.PostgresEpochMicros);
    }
    private static int MakeOffset(string text)
    {
        if (text.Length == 0 || text[0] is not ('+' or '-')) return TemporalContext.Offset(text);
        var sign = text[0] == '-' ? -1 : 1;
        var pieces = text[1..].Split(':');
        if (pieces.Length > 3 || pieces.Any(s => s.Length == 0 || s.Any(c => c is < '0' or > '9'))) throw new SqlException(SqlStates.InvalidParameterValue, "invalid numeric time zone");
        var values = new int[3];
        for (var i = 0; i < pieces.Length; i++)
            if (!int.TryParse(pieces[i], NumberStyles.None, CultureInfo.InvariantCulture, out values[i])) throw new SqlException(SqlStates.InvalidParameterValue, "numeric time zone out of range");
        if (pieces.Length == 1 && pieces[0].Length > 2) { values[1] = values[0] % 100; values[0] /= 100; }
        if (values[0] > 15 || values[1] > 59 || values[2] > 59) throw new SqlException(SqlStates.InvalidParameterValue, "numeric time zone out of range");
        return sign * (values[0] * 3600 + values[1] * 60 + values[2]);
    }
    internal static string Unit(string name, SqlType type, bool truncate = false)
    {
        name = TextEncoding.FoldName(name);
        if (name.Length > DatetimeUnits.TokenMaxLength) name = name[..DatetimeUnits.TokenMaxLength];
        var token = DatetimeUnits.Delta.FirstOrDefault(t => t.Token == name);
        if (token.Token is null && !truncate) token = DatetimeUnits.Date.FirstOrDefault(t => t.Token == name);
        if (token.Kind == "units" || !truncate && token.Kind == "reserv" && token.Value == "epoch") return token.Value;
        if (!truncate && token.Kind == "reserv" && type != SqlType.Interval) throw Errors.Unsupported("temporal field");
        throw new SqlException(SqlStates.InvalidParameterValue, "unrecognized temporal field");
    }
    internal static void ValidateExtract(string field, SqlType type)
    {
        if (type.Kind is not (SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.Date or SqlKind.Interval)) throw new SqlException(SqlStates.UndefinedFunction, "extract requires a temporal source");
        var unit = Unit(field, type);
        if (type == SqlType.Interval && unit is "dow" or "isodow" or "doy" or "isoyear" or "julian" ||
            type == SqlType.Date && unit is "microseconds" or "milliseconds" or "second" or "minute" or "hour" ||
            type != SqlType.Timestamptz && unit.StartsWith("timezone", StringComparison.Ordinal)) throw Errors.Unsupported("temporal field for this type");
    }
    private static bool Monotonic(string unit) => unit is "year" or "decade" or "century" or "millennium" or "julian" or "isoyear" or "epoch";
    private static long PgYear(long year) => year <= 0 ? year - 1 : year;
    private static long Century(long year, long divisor) => year > 0 ? (year + divisor - 1) / divisor : -((divisor - 1 - year) / divisor);
    private static long Decade(long year) => year >= 0 ? year / 10 : -((8 - year) / 10);
    private static (long Year, long Week) IsoWeek(long days)
    {
        var thursday = days + 3 - TemporalValue.FloorMod(days + 3, 7);
        var year = TemporalValue.CivilFromDays(thursday).Year;
        var january4 = TemporalValue.DaysFromCivil(year, 1, 4);
        var firstMonday = january4 - TemporalValue.FloorMod(january4 + 3, 7);
        return (year, (days - firstMonday) / 7 + 1);
    }
    internal static SqlValue Extract(string field, SqlValue source, Meter meter, bool asFloat = false)
    {
        if (asFloat && source.Type == SqlType.Date) source = source.IsNull ? SqlValue.Null(SqlType.Timestamp) : SqlValue.Timestamp(TemporalValue.DateToTimestamp(source.AsDate()));
        var unit = Unit(field, source.Type);
        var infinite = !source.IsNull && (source.Type == SqlType.Date ? TemporalValue.IsInfiniteDate(source.AsDate()) : source.Type.Kind is SqlKind.Timestamp or SqlKind.Timestamptz && TemporalValue.IsInfinite(source.AsTimestamp()));
        // Runtime date_part follows PG's nonfinite special case for timezone fields.
        if (!(asFloat && infinite && source.Type.Kind is SqlKind.Timestamp or SqlKind.Timestamptz)) ValidateExtract(field, source.Type);
        if (source.IsNull) return SqlValue.Null(asFloat ? SqlType.F64 : SqlType.Decimal);
        if (infinite)
        {
            if (!Monotonic(unit)) return SqlValue.Null(asFloat ? SqlType.F64 : SqlType.Decimal);
            if (!asFloat) throw new SqlException(SqlStates.NumericValueOutOfRange, "cannot extract infinite numeric value");
            var negative = source.Type == SqlType.Date ? source.AsDate() < 0 : source.AsTimestamp() < 0;
            return SqlValue.Float(negative ? double.NegativeInfinity : double.PositiveInfinity);
        }
        ExactDecimal value;
        if (source.Type == SqlType.Interval) value = ExtractInterval(unit, source.AsInterval());
        else
        {
            var date = source.Type == SqlType.Date;
            var instant = date ? 0 : source.AsTimestamp();
            var offset = 0;
            if (source.Type == SqlType.Timestamptz && unit != "epoch")
            {
                meter.Charge(Costs.Timezone); meter.Guard();
                var zone = meter.Temporal.ResolveZone(); offset = zone.OffsetAt(instant).Seconds; instant = zone.ToLocal(instant);
            }
            var days = date ? source.AsDate() : TemporalValue.FloorDiv(instant, TemporalValue.MicrosPerDay);
            var time = date ? 0 : TemporalValue.FloorMod(instant, TemporalValue.MicrosPerDay);
            var civil = TemporalValue.CivilFromDays(days);
            var year = PgYear(civil.Year);
            value = unit switch
            {
                "microseconds" => ExactDecimal.FromInt64(time % 60_000_000),
                "milliseconds" => ExactDecimal.FromUnscaled(time % 60_000_000, 3),
                "second" => ExactDecimal.FromUnscaled(time % 60_000_000, 6),
                "minute" => ExactDecimal.FromInt64(time / 60_000_000 % 60),
                "hour" => ExactDecimal.FromInt64(time / 3_600_000_000),
                "day" => ExactDecimal.FromInt64(civil.Day), "month" => ExactDecimal.FromInt64(civil.Month),
                "quarter" => ExactDecimal.FromInt64((civil.Month - 1) / 3 + 1), "year" => ExactDecimal.FromInt64(year),
                "decade" => ExactDecimal.FromInt64(Decade(year)), "century" => ExactDecimal.FromInt64(Century(year, 100)),
                "millennium" => ExactDecimal.FromInt64(Century(year, 1000)), "week" => ExactDecimal.FromInt64(IsoWeek(days).Week),
                "isoyear" => ExactDecimal.FromInt64(PgYear(IsoWeek(days).Year)), "dow" => ExactDecimal.FromInt64(TemporalValue.FloorMod(days + 4, 7)),
                "isodow" => ExactDecimal.FromInt64(TemporalValue.FloorMod(days + 3, 7) + 1),
                "doy" => ExactDecimal.FromInt64(days - TemporalValue.DaysFromCivil(civil.Year, 1, 1) + 1),
                "epoch" => date ? ExactDecimal.FromInt64(days * 86400) : ExactDecimal.FromUnscaled(instant, 6),
                "julian" => date ? ExactDecimal.FromInt64(days + 2440588) : ExactDecimal.FromInt64(days + 2440588).Add(ExactDecimal.FromInt64(time).Divide(ExactDecimal.FromInt64(TemporalValue.MicrosPerDay))),
                "timezone" => ExactDecimal.FromInt64(offset), "timezone_hour" => ExactDecimal.FromInt64(offset / 3600),
                "timezone_minute" => ExactDecimal.FromInt64(offset / 60 % 60),
                _ => throw Errors.Unsupported("temporal field")
            };
            if (asFloat && unit == "julian") return SqlValue.Float(days + 2440588d + (time / 1_000_000 + (time % 1_000_000) / 1e6) / 86400d);
        }
        return asFloat ? SqlValue.Float(FloatValue.Parse(value.ToString())) : SqlValue.Decimal(value);
    }
    private static ExactDecimal ExtractInterval(string unit, SqlInterval interval)
    {
        var seconds = interval.Micros % 60_000_000;
        return unit switch
        {
            "microseconds" => ExactDecimal.FromInt64(seconds), "milliseconds" => ExactDecimal.FromUnscaled(seconds, 3), "second" => ExactDecimal.FromUnscaled(seconds, 6),
            "minute" => ExactDecimal.FromInt64(interval.Micros / 60_000_000 % 60), "hour" => ExactDecimal.FromInt64(interval.Micros / 3_600_000_000),
            "day" => ExactDecimal.FromInt64(interval.Days), "week" => ExactDecimal.FromInt64(interval.Days / 7),
            "month" => ExactDecimal.FromInt64(interval.Months % 12), "year" => ExactDecimal.FromInt64(interval.Months / 12),
            "decade" => ExactDecimal.FromInt64(interval.Months / 120), "century" => ExactDecimal.FromInt64(interval.Months / 1200), "millennium" => ExactDecimal.FromInt64(interval.Months / 12000),
            "quarter" => ExactDecimal.FromInt64(interval.Months >= 0 ? interval.Months % 12 / 3 + 1 : -((-(long)interval.Months % 12) / 3 + 1)),
            "epoch" => ExactDecimal.FromInt64((long)(interval.Months / 12) * 31557600 + (long)(interval.Months % 12) * 2592000 + (long)interval.Days * 86400).Add(ExactDecimal.FromUnscaled(interval.Micros, 6)),
            _ => throw Errors.Unsupported("interval field")
        };
    }
    internal static SqlValue Truncate(string field, SqlValue source, Meter meter, string? explicitZone = null)
    {
        var unit = Unit(field, source.Type, truncate: true);
        if (unit.StartsWith("timezone", StringComparison.Ordinal) || source.Type == SqlType.Interval && unit == "week") throw Errors.Unsupported("temporal truncation unit");
        if (source.IsNull) return source;
        if (source.Type == SqlType.Interval)
        {
            var interval = source.AsInterval();
            var step = unit switch { "microseconds" => 1L, "milliseconds" => 1000L, "second" => 1_000_000L, "minute" => 60_000_000L, "hour" => 3_600_000_000L, _ => 0L };
            if (step != 0) return SqlValue.Interval(new(interval.Months, interval.Days, interval.Micros / step * step));
            if (unit == "day") return SqlValue.Interval(new(interval.Months, interval.Days, 0));
            var monthStep = unit switch { "month" => 1, "quarter" => 3, "year" => 12, "decade" => 120, "century" => 1200, "millennium" => 12000, _ => throw Errors.Unsupported("interval truncation unit") };
            return SqlValue.Interval(new(interval.Months / monthStep * monthStep, 0, 0));
        }
        var micros = source.AsTimestamp();
        if (TemporalValue.IsInfinite(micros)) return source;
        TimeZoneRule? truncationZone = null;
        if (source.Type == SqlType.Timestamptz)
        {
            truncationZone = meter.Temporal.ResolveZone(explicitZone);
            meter.Charge(Costs.Timezone); meter.Guard(); micros = truncationZone.ToLocal(micros);
        }
        SqlValue Rebuild(long local) => SqlValue.Timestamp(truncationZone is null ? local : truncationZone.ToInstant(local), source.Type == SqlType.Timestamptz);
        var subdayStep = unit switch { "microseconds" => 1L, "milliseconds" => 1000L, "second" => 1_000_000L, "minute" => 60_000_000L, "hour" => 3_600_000_000L, "day" => TemporalValue.MicrosPerDay, _ => 0L };
        if (subdayStep != 0) return Rebuild(TemporalValue.FiniteTimestamp((Int128)micros - TemporalValue.FloorMod(micros, subdayStep)));
        var days = TemporalValue.FloorDiv(micros, TemporalValue.MicrosPerDay);
        var civil = TemporalValue.CivilFromDays(days);
        var year = civil.Year; var month = civil.Month;
        if (unit == "week") days -= TemporalValue.FloorMod(days + 3, 7);
        else
        {
            if (unit == "quarter") month = (month - 1) / 3 * 3 + 1;
            else if (unit != "month")
            {
                month = 1;
                var pgYear = PgYear(year);
                var start = unit switch
                {
                    "year" => pgYear,
                    "decade" => Decade(pgYear) * 10 - (Decade(pgYear) < 1 ? 1 : 0),
                    "century" => Century(pgYear, 100) < 1 ? Century(pgYear, 100) * 100 : (Century(pgYear, 100) - 1) * 100 + 1,
                    "millennium" => Century(pgYear, 1000) < 1 ? Century(pgYear, 1000) * 1000 : (Century(pgYear, 1000) - 1) * 1000 + 1,
                    _ => throw Errors.Unsupported("timestamp truncation unit")
                };
                year = start < 0 ? start + 1 : start;
            }
            days = TemporalValue.DaysFromCivil(year, month, 1);
        }
        return Rebuild(TemporalValue.FiniteTimestamp((Int128)days * TemporalValue.MicrosPerDay));
    }
}
