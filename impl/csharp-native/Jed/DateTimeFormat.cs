using System.Globalization;
using System.Text;

namespace Jed;

internal static class DateTimeFormat
{
    internal static SqlValue Render(SqlValue value, string template, Meter meter)
    {
        if (template.Length == 0 || value.Type != SqlType.Interval && TemporalValue.IsInfinite(value.AsTimestamp())) return SqlValue.Null(SqlType.Text);
        var interval = value.Type == SqlType.Interval;
        long year, month, day, hour, minute, second, micro, days, dayOfYear;
        var offset = 0; var abbreviation = "";
        if (interval)
        {
            var source = value.AsInterval(); year = source.Months / 12; month = source.Months % 12; day = source.Days;
            hour = source.Micros / 3_600_000_000; minute = source.Micros / 60_000_000 % 60; second = source.Micros / 1_000_000 % 60; micro = source.Micros % 1_000_000;
            dayOfYear = source.Months * 30L + day;
            days = Julian((int)year, (int)month, (int)day) - 2440588L;
        }
        else
        {
            var instant = value.AsTimestamp(); var wall = instant;
            if (value.Type == SqlType.Timestamptz)
            {
                meter.Charge(Costs.Timezone); meter.Guard();
                var zone = meter.Temporal.ResolveZone(); var info = zone.OffsetAt(instant); offset = info.Seconds;
                abbreviation = zone.Version.Length == 0 && zone.Name != "UTC" ? "" : info.Abbreviation;
                wall = zone.ToLocal(instant);
            }
            days = TemporalValue.FloorDiv(wall, TemporalValue.MicrosPerDay);
            var civil = TemporalValue.CivilFromDays(days); year = civil.Year; month = civil.Month; day = civil.Day;
            var time = TemporalValue.FloorMod(wall, TemporalValue.MicrosPerDay);
            hour = time / 3_600_000_000; minute = time / 60_000_000 % 60; second = time / 1_000_000 % 60; micro = time % 1_000_000;
            dayOfYear = days - TemporalValue.DaysFromCivil(year, 1, 1) + 1;
        }
        var weekday = TemporalValue.FloorMod(days + 4, 7); var isoDay = weekday == 0 ? 7 : weekday;
        var isoYear = TemporalValue.CivilFromDays(days + 4 - isoDay).Year;
        var isoStart = IsoStart(isoYear); var isoDayOfYear = days - isoStart + 1;
        var output = new StringBuilder();
        foreach (var node in FormatTemplate.Parse(template))
        {
            if (node.Literal) { output.Append(node.Text); continue; }
            var id = node.Id; var upper = id.ToUpperInvariant();
            if (interval && upper is "MONTH" or "MON" or "DAY" or "DY" or "D" or "ID" or "AD" or "BC" or "A_D" or "B_C" or "TZ" or "TZH" or "TZM" or "OF") throw TemporalValue.Invalid();
            string? text = null; long number = 0; var width = 0; var signedYear = false;
            switch (upper)
            {
                case "FX": continue;
                case "HH": case "HH12": number = hour % 12; if (number == 0) number = 12; width = 2; signedYear = true; break;
                case "HH24": number = hour; width = 2; signedYear = true; break;
                case "MI": number = minute; width = 2; signedYear = true; break;
                case "SS": number = second; width = 2; signedYear = true; break;
                case "MS": number = micro / 1000; width = 3; signedYear = true; break;
                case "US": number = micro; width = 6; signedYear = true; break;
                case "FF1": case "FF2": case "FF3": case "FF4": case "FF5": case "FF6":
                    width = upper[2] - '0'; number = micro; for (var i = width; i < 6; i++) number /= 10; signedYear = true; break;
                case "SSSS": number = hour * 3600 + minute * 60 + second; break;
                case "AM": case "PM": case "A_M": case "P_M":
                    text = hour % 24 >= 12 ? "PM" : "AM"; if (upper.Contains('_')) text = text[0] + ".M."; text = Case(text, id); break;
                case "AD": case "BC": case "A_D": case "B_C":
                    text = year <= 0 ? "BC" : "AD"; if (upper.Contains('_')) text = text[0] + "." + text[1] + "."; text = Case(text, id); break;
                case "YYYY": number = DisplayYear(year, interval); width = 4; signedYear = true; break;
                case "YYY": number = DisplayYear(year, interval) % 1000; width = 3; break;
                case "YY": number = DisplayYear(year, interval) % 100; width = 2; break;
                case "Y": number = DisplayYear(year, interval) % 10; break;
                case "Y_YYY": var display = DisplayYear(year, interval); text = (display / 1000).ToString(CultureInfo.InvariantCulture) + "," + Number(display % 1000, 3, false); break;
                case "IYYY": number = DisplayYear(isoYear, interval); width = 4; signedYear = true; break;
                case "IYY": number = DisplayYear(isoYear, interval) % 1000; width = 3; break;
                case "IY": number = DisplayYear(isoYear, interval) % 100; width = 2; break;
                case "I": number = DisplayYear(isoYear, interval) % 10; break;
                case "CC": number = interval ? year / 100 : year > 0 ? (year - 1) / 100 + 1 : -(99 - (year - 1)) / 100; width = 2; signedYear = true; break;
                case "MM": number = month; width = 2; signedYear = true; break;
                case "MONTH": text = Case(FormattingCatalog.MonthsFull[(int)month - 1], id); if (!node.Fill && !node.Translate) text = text.PadRight(9); break;
                case "MON": text = Case(FormattingCatalog.MonthsAbbrev[(int)month - 1], id); break;
                case "RM":
                    var romanMonth = month > 0 ? (int)month : month < 0 ? (int)(12 + month + 1) : year > 0 ? 12 : year < 0 ? 1 : 0;
                    text = romanMonth == 0 ? "" : FormattingCatalog.RmMonthsUpper[12 - romanMonth];
                    if (id == "rm") text = text.ToLowerInvariant(); if (!node.Fill && text.Length != 0) text = text.PadRight(4); break;
                case "DAY": text = Case(FormattingCatalog.DaysFull[(int)weekday], id); if (!node.Fill && !node.Translate) text = text.PadRight(9); break;
                case "DY": text = Case(FormattingCatalog.DaysAbbrev[(int)weekday], id); break;
                case "DDD": number = dayOfYear; width = 3; break;
                case "IDDD": number = isoDayOfYear; width = 3; break;
                case "DD": number = day; width = 2; break;
                case "D": number = weekday + 1; break;
                case "ID": number = isoDay; break;
                case "W": number = (day - 1) / 7 + 1; break;
                case "WW": number = (dayOfYear - 1) / 7 + 1; width = 2; break;
                case "IW": number = (isoDayOfYear - 1) / 7 + 1; width = 2; break;
                case "Q": number = (month - 1) / 3 + 1; break;
                case "J": number = unchecked((int)(days + 2440588)); break;
                case "TZ": text = id == "tz" ? abbreviation.ToLowerInvariant() : abbreviation; break;
                case "TZH": text = (offset < 0 ? "-" : "+") + Number(Math.Abs(offset / 3600), node.Fill ? 0 : 2, false); break;
                case "TZM": number = Math.Abs(offset / 60 % 60); width = 2; break;
                case "OF": text = (offset < 0 ? "-" : "+") + Number(Math.Abs(offset / 3600), node.Fill ? 0 : 2, false); if (offset / 60 % 60 != 0) text += ":" + Number(Math.Abs(offset / 60 % 60), 2, false); break;
                default: throw Errors.Unsupported($"datetime format pattern {id}");
            }
            if (text is null)
            {
                text = Number(number, node.Fill ? 0 : width, signedYear);
                if (node.Suffix is "TH" or "th") text += FormatTemplate.Ordinal(number, node.Suffix == "TH");
            }
            output.Append(text);
        }
        return SqlValue.Text(output.ToString());
    }
    private static long DisplayYear(long year, bool interval) => !interval && year <= 0 ? 1 - year : year;
    internal static string Number(long value, int width, bool signExtra)
    {
        var raw = value.ToString(CultureInfo.InvariantCulture);
        if (value >= 0) return raw.PadLeft(width, '0');
        return "-" + raw[1..].PadLeft(Math.Max(0, width - (signExtra ? 0 : 1)), '0');
    }
    private static string Case(string text, string pattern) => pattern == pattern.ToUpperInvariant() ? text.ToUpperInvariant() : pattern == pattern.ToLowerInvariant() ? text.ToLowerInvariant() : char.ToUpperInvariant(text[0]) + text[1..].ToLowerInvariant();
    internal static long IsoStart(long year)
    {
        var january4 = TemporalValue.DaysFromCivil(year, 1, 4);
        return january4 - TemporalValue.FloorMod(january4 + 3, 7);
    }
    // PostgreSQL's calendar helper intentionally uses wrapping C-int arithmetic.
    internal static int Julian(int year, int month, int day)
    {
        unchecked
        {
            if (month > 2) { month += 1; year += 4800; } else { month += 13; year += 4799; }
            var century = year / 100;
            var julian = year * 365 - 32167;
            julian += year / 4 - century + century / 4;
            return julian + 7834 * month / 256 + day;
        }
    }
}
