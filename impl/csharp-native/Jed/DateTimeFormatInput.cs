using System.Globalization;
using System.Text;

namespace Jed;

internal static class DateTimeFormatInput
{
    internal static SqlValue Parse(string input, string template, bool dateOnly, Meter meter)
    {
        var nodes = FormatTemplate.Parse(template); var source = Encoding.UTF8.GetBytes(input); var position = 0;
        var fields = new Dictionary<string, long>(StringComparer.Ordinal); var convention = "none"; var fixedMode = false; var precision = 6;
        var hour12 = false; var yearWidth = 0; var offsetSign = 1; var hasOffset = false; var swallowedMinus = false;
        void Set(string field, long value)
        {
            if (fields.TryGetValue(field, out var old) && old != value) throw new SqlException("22007", $"conflicting values for \"{field}\" field in formatting string");
            fields[field] = value;
        }
        long Get(string field, long fallback = 0) => fields.GetValueOrDefault(field, fallback);
        void Spaces() { while (position < source.Length && source[position] is 9 or 10 or 11 or 12 or 13 or 32) position++; }
        static bool Digit(byte value) => value is >= (byte)'0' and <= (byte)'9';
        for (var nodeIndex = 0; nodeIndex < nodes.Length && position < source.Length; nodeIndex++)
        {
            var node = nodes[nodeIndex]; var upper = node.Id.ToUpperInvariant();
            if (upper == "FX") { fixedMode = true; continue; }
            if (node.Literal)
            {
                var ascii = node.Text.Length == 1 && node.Text[0] < 128;
                var separator = ascii && !char.IsAsciiLetterOrDigit(node.Text[0]);
                if (fixedMode || !separator)
                {
                    if (position < source.Length) position += RuneLength(source[position]);
                }
                else
                {
                    var before = position; Spaces();
                    if (position < source.Length && source[position] < 128 && !char.IsAsciiLetterOrDigit((char)source[position]))
                    {
                        swallowedMinus = source[position] == '-'; position++;
                    }
                    else if (position != before) swallowedMinus = false;
                }
                continue;
            }
            Spaces(); if (position >= source.Length) break;
            if (node.Mode != "none")
            {
                if (convention != "none" && convention != node.Mode) throw new SqlException("22007", "invalid combination of date conventions");
                convention = node.Mode;
            }
            switch (upper)
            {
                case "AM": case "PM": case "A_M": case "P_M": Set("pm", ReadWord(upper.Contains('_') ? ["a.m.", "p.m."] : ["am", "pm"])); hour12 = true; break;
                case "AD": case "BC": case "A_D": case "B_C": Set("bc", ReadWord(upper.Contains('_') ? ["a.d.", "b.c."] : ["ad", "bc"])); break;
                case "MONTH": Set("month", ReadWord(FormattingCatalog.MonthsFull) + 1); break;
                case "MON": Set("month", ReadWord(FormattingCatalog.MonthsAbbrev) + 1); break;
                case "DAY": var fullDay = ReadWord(FormattingCatalog.DaysFull); Set("week_day", fullDay == 0 ? 7 : fullDay); break;
                case "DY": var shortDay = ReadWord(FormattingCatalog.DaysAbbrev); Set("week_day", shortDay == 0 ? 7 : shortDay); break;
                case "RM": Set("month", 12 - ReadWord(FormattingCatalog.RmMonthsUpper)); break;
                case "TZ": case "OF":
                    if (upper == "TZ" && char.IsAsciiLetter((char)source[position])) throw Errors.Unsupported("time zone abbreviations are not supported for input");
                    offsetSign = source[position] == '-' || swallowedMinus ? -1 : 1;
                    if (source[position] is (byte)'+' or (byte)'-') position++;
                    Set("zone_hour", ReadInteger(2, false, out _)); hasOffset = true;
                    if (position < source.Length && source[position] == ':') { position++; Set("zone_minute", ReadInteger(2, false, out _)); }
                    break;
                case "TZH":
                    offsetSign = source[position] == '-' || swallowedMinus ? -1 : 1;
                    if (source[position] is (byte)'+' or (byte)'-') position++;
                    Set("zone_hour", ReadInteger(2, false, out _)); hasOffset = true; break;
                case "TZM": Set("zone_minute", ReadInteger(2, false, out _)); hasOffset = true; break;
                case "Y_YYY":
                    var thousands = ReadInteger(0, false, out _);
                    if (position >= source.Length || source[position++] != ',') throw TemporalValue.Invalid();
                    Set("year", thousands * 1000 + ReadInteger(3, false, out _)); yearWidth = 4; break;
                default:
                    var width = upper switch { "HH" or "HH12" or "HH24" => 2, "SSSS" => 0, "MS" => 3, "US" => 6, "Y" or "I" => 1, "YY" or "IY" => 2, "YYY" or "IYY" => 3, "YYYY" or "IYYY" => 4, "IDDD" => 3, "ID" => 1, _ => node.Text.Length };
                    if (upper.StartsWith("FF", StringComparison.Ordinal)) width = upper[2] - '0';
                    var strictWidth = !node.Fill && nodeIndex + 1 < nodes.Length && nodes[nodeIndex + 1].Digit;
                    var value = ReadInteger(width, strictWidth, out var digits);
                    switch (upper)
                    {
                        case "YYYY": case "YYY": case "YY": case "Y": case "IYYY": case "IYY": case "IY": case "I": Set("year", value); yearWidth = width; break;
                        case "CC": Set("century", value); break;
                        case "MM": Set("month", value); break;
                        case "DD": Set("day", value); break;
                        case "DDD": case "IDDD": Set("year_day", value); break;
                        case "WW": case "IW": Set("year_week", value); break;
                        case "W": Set("month_week", value); break;
                        case "D": if (value is < 1 or > 7) throw TemporalValue.Overflow(); Set("week_day", value == 1 ? 7 : value - 1); break;
                        case "ID": if (value is < 1 or > 7) throw TemporalValue.Overflow(); Set("week_day", value); break;
                        case "J": Set("julian", value); break;
                        case "Q": break;
                        case "HH": case "HH12": hour12 = true; Set("hour", value); break;
                        case "HH24": Set("hour", value); break;
                        case "MI": Set("minute", value); break;
                        case "SS": Set("second", value); break;
                        case "SSSS": Set("day_second", value); break;
                        case "MS": if (digits < 3) for (var i = digits; i < 3; i++) value *= 10; Set("millis", value); break;
                        case "US": case "FF1": case "FF2": case "FF3": case "FF4": case "FF5": case "FF6":
                            if (digits < 6) for (var i = digits; i < 6; i++) value *= 10; Set("micros", value);
                            if (upper.StartsWith("FF", StringComparison.Ordinal)) precision = width; break;
                        default: throw TemporalValue.Invalid();
                    }
                    break;
            }
            if (node.Suffix is "TH" or "th") position = Math.Min(source.Length, position + 2);
            swallowedMinus = false;
        }
        var year = Get("year");
        if (fields.ContainsKey("century") && yearWidth <= 2)
        {
            var century = Get("century"); year = century > 0 ? (century - 1) * 100 + (fields.ContainsKey("year") ? year == 0 ? 100 : year : 1) : century * 100 + (fields.ContainsKey("year") ? year == 0 ? 100 : year : 1);
        }
        else if (yearWidth is > 0 and < 4 && year >= 0)
            year += year < 70 ? 2000 : year < 100 ? 1900 : year < 520 ? 2000 : year < 1000 ? 1000 : 0;
        if (Get("bc") != 0)
        {
            if (fields.ContainsKey("century") && !fields.ContainsKey("year")) year = Get("century") * 100;
            year = -year;
        }
        if (year < 0) year++;
        var month = Get("month", 1); var day = Get("day", 1); long days;
        if (Get("julian") != 0) days = Get("julian") - 2440588;
        else if (convention == "isoweek")
        {
            var start = DateTimeFormat.IsoStart(year);
            days = fields.ContainsKey("year_day") ? start + Get("year_day") - 1 : start + (Get("year_week", 1) - 1) * 7 + Get("week_day", 1) - 1;
        }
        else
        {
            if (fields.ContainsKey("year_day") || fields.ContainsKey("year_week"))
            {
                if (!fields.ContainsKey("year")) throw TemporalValue.Invalid();
                var yearDay = fields.ContainsKey("year_day") ? Get("year_day") : (Get("year_week") - 1) * 7 + 1;
                if (yearDay < 1 || yearDay > (TemporalValue.DaysInMonth(year, 2) == 29 ? 366 : 365)) throw TemporalValue.Overflow();
                days = TemporalValue.DaysFromCivil(year, 1, 1) + yearDay - 1;
            }
            else
            {
                if (fields.ContainsKey("month_week")) day = (Get("month_week") - 1) * 7 + 1;
                if (!fields.ContainsKey("year") && !fields.ContainsKey("century"))
                {
                    if (month is < 1 or > 12 || day is < 1 or > 31) throw TemporalValue.Overflow();
                    days = TemporalValue.DaysFromCivil(year, (int)month, 1) + day - 1;
                }
                else { TemporalValue.ValidateCivil(year, month, day); days = TemporalValue.DaysFromCivil(year, (int)month, (int)day); }
            }
        }
        var hour = Get("hour", Get("day_second") / 3600); var minute = Get("minute", Get("day_second") / 60 % 60); var second = Get("second", Get("day_second") % 60);
        if (hour12) { if (hour is < 1 or > 12) throw TemporalValue.Invalid(); hour = hour % 12 + Get("pm") * 12; }
        var micros = Get("micros") + Get("millis") * 1000;
        if (hour is < 0 or > 23 || minute is < 0 or > 59 || second is < 0 or > 59 || micros is < 0 or > 999999) throw TemporalValue.Overflow();
        if (dateOnly) return SqlValue.Date(TemporalValue.FiniteDate(days));
        var wall = TemporalValue.FiniteTimestamp((Int128)days * TemporalValue.MicrosPerDay + hour * 3_600_000_000 + minute * 60_000_000 + second * 1_000_000 + micros);
        long instant;
        if (hasOffset)
        {
            var zoneHour = Get("zone_hour"); var zoneMinute = Get("zone_minute");
            if (zoneHour is < 0 or > 15 || zoneMinute is < 0 or > 59) throw new SqlException("22009", "time zone displacement out of range");
            instant = TemporalValue.FiniteTimestamp((Int128)wall - offsetSign * (zoneHour * 3600 + zoneMinute * 60) * 1_000_000);
        }
        else { meter.Charge(Costs.Timezone); meter.Guard(); instant = meter.Temporal.ResolveZone().ToInstant(wall); }
        return SqlValue.Timestamp(TemporalValue.RoundTimestamp(instant, precision), withZone: true);

        int ReadWord(string[] words)
        {
            for (var i = 0; i < words.Length; i++)
            {
                var word = words[i]; if (position + word.Length > source.Length) continue;
                var same = true;
                for (var j = 0; j < word.Length; j++)
                    if (source[position + j] >= 128 || char.ToUpperInvariant((char)source[position + j]) != char.ToUpperInvariant(word[j])) { same = false; break; }
                if (!same) continue;
                position += word.Length; return i;
            }
            throw TemporalValue.Invalid();
        }
        long ReadInteger(int width, bool strict, out int digits)
        {
            Spaces(); var start = position; var negative = false;
            if (position < source.Length && source[position] is (byte)'+' or (byte)'-') { negative = source[position++] == '-'; }
            var firstDigit = position; long value = 0; var end = strict ? Math.Min(source.Length, start + width) : source.Length;
            while (position < end && Digit(source[position]))
            {
                value = value * 10 + source[position++] - '0'; if (value > 2147483648L) throw TemporalValue.Overflow();
            }
            digits = position - firstDigit;
            if (digits == 0 || strict && position - start != width) throw TemporalValue.Invalid();
            value = negative ? -value : value; if (value < int.MinValue || value > int.MaxValue) throw TemporalValue.Overflow();
            return value;
        }
    }
    private static int RuneLength(byte value) => value < 128 ? 1 : value < 224 ? 2 : value < 240 ? 3 : 4;
}
