namespace Jed;

internal sealed class PosixZone
{
    private readonly TimeZoneOffset standard, daylight;
    private readonly Rule? start, end;
    private readonly record struct Rule(int Month, int Week, int Weekday, int Seconds);
    private PosixZone(TimeZoneOffset standard, TimeZoneOffset daylight, Rule? start, Rule? end)
    { this.standard = standard; this.daylight = daylight; this.start = start; this.end = end; }
    internal int[] Offsets => start is null ? [standard.Seconds] : [standard.Seconds, daylight.Seconds];
    internal static PosixZone Parse(string input)
    {
        var reader = new Parser(input); var standardName = reader.Name(); var standardOffset = -reader.Offset();
        var standard = new TimeZoneOffset(standardOffset, standardName, false);
        if (reader.End) return new(standard, standard, null, null);
        var daylightName = reader.Name(); var daylightOffset = reader.End || reader.Peek == ',' ? standardOffset + 3600 : -reader.Offset();
        if (reader.End) throw TimeZoneRule.Bad("DST footer requires transition rules");
        reader.Expect(','); var start = reader.Rule(); reader.Expect(','); var end = reader.Rule();
        if (!reader.End) throw TimeZoneRule.Bad("trailing POSIX zone footer");
        return new(standard, new(daylightOffset, daylightName, true), start, end);
    }
    internal TimeZoneOffset OffsetAt(long seconds)
    {
        if (start is null) return standard;
        var year = TemporalValue.CivilFromDays(TemporalValue.FloorDiv(seconds, 86400)).Year;
        var beginning = Instant(start.Value, year, standard.Seconds); var finish = Instant(end!.Value, year, daylight.Seconds);
        var active = beginning < finish ? seconds >= beginning && seconds < finish : seconds >= beginning || seconds < finish;
        return active ? daylight : standard;
    }
    internal IEnumerable<(long Instant, int Before, int After)> Transitions(long year)
    {
        if (start is null) yield break;
        yield return (Instant(start.Value, year, standard.Seconds), standard.Seconds, daylight.Seconds);
        yield return (Instant(end!.Value, year, daylight.Seconds), daylight.Seconds, standard.Seconds);
    }
    private static long Instant(Rule rule, long year, int before)
    {
        var first = TemporalValue.DaysFromCivil(year, rule.Month, 1);
        var weekday = (int)TemporalValue.FloorMod(first + 4, 7);
        var day = 1 + (rule.Weekday - weekday + 7) % 7 + (rule.Week - 1) * 7;
        if (day > TemporalValue.DaysInMonth(year, rule.Month)) day -= 7;
        return (first + day - 1) * 86400 + rule.Seconds - before;
    }
    private sealed class Parser(string input)
    {
        private int position;
        internal bool End => position == input.Length;
        internal char Peek => End ? '\0' : input[position];
        internal void Expect(char character) { if (Peek != character) throw TimeZoneRule.Bad("invalid POSIX zone rule"); position++; }
        internal string Name()
        {
            if (Peek == '<')
            {
                position++; var begin = position;
                while (!End && Peek != '>') position++;
                if (End || position - begin < 3) throw TimeZoneRule.Bad("invalid POSIX abbreviation");
                var name = input[begin..position]; position++; return name;
            }
            var first = position;
            while (Peek is >= 'A' and <= 'Z' or >= 'a' and <= 'z') position++;
            if (position - first < 3) throw TimeZoneRule.Bad("invalid POSIX abbreviation");
            return input[first..position];
        }
        private int Number(int maximum)
        {
            if (Peek is < '0' or > '9') throw TimeZoneRule.Bad("invalid POSIX number");
            var value = 0;
            while (Peek is >= '0' and <= '9')
            { var digit = input[position++] - '0'; if (value > (maximum - digit) / 10) throw TimeZoneRule.Bad("POSIX number out of range"); value = value * 10 + digit; if (value > maximum) throw TimeZoneRule.Bad("POSIX number out of range"); }
            return value;
        }
        internal int Offset()
        {
            var sign = 1; if (Peek is '+' or '-') { if (Peek == '-') sign = -1; position++; }
            var hour = Number(167); var minute = 0; var second = 0;
            if (Peek == ':') { position++; minute = Number(59); if (Peek == ':') { position++; second = Number(60); } }
            return sign * (hour * 3600 + minute * 60 + second);
        }
        internal Rule Rule()
        {
            Expect('M'); var month = Number(12); Expect('.'); var week = Number(5); Expect('.'); var weekday = Number(6);
            if (month == 0 || week == 0) throw TimeZoneRule.Bad("invalid POSIX month/week");
            var seconds = 7200; if (Peek == '/') { position++; seconds = Offset(); }
            return new(month, week, weekday, seconds);
        }
    }
}
