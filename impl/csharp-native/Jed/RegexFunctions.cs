using System.Text;

namespace Jed;

internal static class RegexFunctions
{
    internal static SqlValue Call(Resolved expression, SqlValue[] args, Meter meter, List<SqlValue>? rows = null)
    {
        if (args.Any(arg => arg.IsNull)) return SqlValue.Null(expression.Type);
        var descriptor = expression.Regex!; var name = descriptor.Name;
        var start = 1L; var occurrence = 1L; var group = 0L; var endOption = 0L;
        if (name is "regexp_count" or "regexp_substr" or "regexp_instr")
        {
            if (args.Length > 2) start = args[2].AsInteger(); Validate("start", start, start >= 1);
            if (name != "regexp_count" && args.Length > 3) occurrence = args[3].AsInteger(); Validate("N", occurrence, occurrence >= 1);
            if (name == "regexp_instr" && args.Length > 4) endOption = args[4].AsInteger(); Validate("endoption", endOption, endOption is 0 or 1);
            var groupIndex = name == "regexp_instr" ? 6 : 5;
            if (name != "regexp_count" && args.Length > groupIndex) group = args[groupIndex].AsInteger(); Validate("subexpr", group, group >= 0);
        }
        var flags = descriptor.FlagsIndex < 0 ? descriptor.Flags : args[descriptor.FlagsIndex].AsText(); RegexResolve.ValidateFlags(name, flags);
        var program = descriptor.Program; RegexMachine machine;
        if (program is not null)
        {
            if (!meter.RegexMachines.TryGetValue(descriptor, out machine!))
            {
                meter.Charge(program.Code.Length * Costs.RegexCompile); meter.Guard(); machine = new(program); meter.RegexMachines.Add(descriptor, machine);
            }
        }
        else
        {
            program = RegexProgram.Compile(args[1].AsText(), flags, meter.Unicode); meter.Charge(program.Code.Length * Costs.RegexCompile); meter.Guard(); machine = new(program);
        }
        var source = args[0].AsText(); var input = program.FoldSubject(source).EnumerateRunes().Select(rune => rune.Value).ToArray();
        var offsets = new int[input.Length + 1]; var point = 0; foreach (var rune in source.EnumerateRunes()) { offsets[point + 1] = offsets[point] + rune.Utf16SequenceLength; point++; }
        string Slice(int first, int last) => source[offsets[first]..offsets[last]];
        var at = start > input.Length + 1L ? input.Length + 1 : (int)start - 1;
        var matches = new List<int[]>(); long count = 0;
        var all = name is "regexp_count" or "regexp_split_to_array" or "regexp_split_to_table" || name is "regexp_replace" or "regexp_matches" && flags.Contains('g');
        while (at <= input.Length)
        {
            var captures = machine.Search(input, at, meter).Captures; if (captures is null) break; count++;
            if (all || count == occurrence) matches.Add(captures);
            if (!all && count == occurrence) break;
            at = captures[1] + (captures[0] == captures[1] ? 1 : 0);
        }
        if (name is "~" or "~*" or "!~" or "!~*" or "regexp_like") return SqlValue.Boolean((matches.Count != 0) != name.StartsWith('!'));
        if (name == "regexp_count") return SqlValue.Integer(count, SqlType.I32);
        if (name is "regexp_split_to_array" or "regexp_split_to_table")
        {
            var pieces = new List<SqlValue>(); var previousEnd = 0; var end = 0;
            foreach (var captures in matches)
            {
                if (captures[0] != input.Length && captures[1] > previousEnd) { pieces.Add(SqlValue.Text(Slice(end, captures[0]))); end = captures[1]; }
                previousEnd = captures[1];
            }
            pieces.Add(SqlValue.Text(Slice(end, input.Length))); rows?.AddRange(pieces); return SqlValue.Array(new(SqlType.Text, pieces));
        }
        if (name == "regexp_replace")
        {
            var replacement = args[2].AsText(); var segments = new List<(string Source, int Start, int Length)>(); var previous = 0; long bytes = 0;
            void Segment(string text, int first, int last) { bytes += Encoding.UTF8.GetByteCount(text.AsSpan(first, last - first)); segments.Add((text, first, last - first)); }
            foreach (var captures in matches)
            {
                meter.Charge(TextEncoding.ByteCount(replacement) * Costs.ScalarByte); meter.Guard(); Segment(source, offsets[previous], offsets[captures[0]]);
                for (var i = 0; i < replacement.Length; i++)
                {
                    if (replacement[i] == '\\' && i + 1 < replacement.Length)
                    {
                        var next = replacement[i + 1]; var selected = next == '&' ? 0 : next is >= '1' and <= '9' ? next - '0' : -1;
                        if (selected >= 0) { i++; if (selected * 2 + 1 < captures.Length && captures[selected * 2] >= 0) Segment(source, offsets[captures[selected * 2]], offsets[captures[selected * 2 + 1]]); continue; }
                        if (next == '\\') { i++; Segment(replacement, i, i + 1); continue; }
                    }
                    var width = char.IsHighSurrogate(replacement[i]) ? 2 : 1; Segment(replacement, i, i + width); i += width - 1;
                }
                previous = captures[1];
            }
            Segment(source, offsets[previous], source.Length); meter.ScalarOutput(bytes);
            var output = new StringBuilder(); foreach (var segment in segments) output.Append(segment.Source, segment.Start, segment.Length); return SqlValue.Text(output.ToString());
        }
        if (matches.Count == 0) return name == "regexp_instr" ? SqlValue.Integer(0, SqlType.I32) : SqlValue.Null(expression.Type);
        var match = matches[0];
        if (name is "regexp_match" or "regexp_matches")
        {
            var first = program.Groups == 0 ? 0 : 1; long total = 0;
            foreach (var capture in matches)
            {
                long bytes = 16L + 32L * (program.Groups - first + 1);
                for (var i = first; i <= program.Groups; i++) if (capture[i * 2] >= 0) bytes += Encoding.UTF8.GetByteCount(source.AsSpan(offsets[capture[i * 2]], offsets[capture[i * 2 + 1]] - offsets[capture[i * 2]]));
                meter.Charge(bytes * Costs.ScalarByte); meter.Guard(); total += bytes;
            }
            meter.ScalarReserve(total);
            SqlValue result = default;
            foreach (var capture in matches)
            {
                var values = new List<SqlValue>(); for (var i = first; i <= program.Groups; i++) values.Add(capture[i * 2] < 0 ? SqlValue.Null(SqlType.Text) : SqlValue.Text(Slice(capture[i * 2], capture[i * 2 + 1])));
                result = SqlValue.Array(new(SqlType.Text, values)); rows?.Add(result);
            }
            return result;
        }
        if (name == "substring") group = program.Groups == 0 ? 0 : 1;
        if (program.Groups == 0 && group == 1) group = 0;
        if (group > program.Groups || match[(int)group * 2] < 0) return name == "regexp_instr" ? SqlValue.Integer(0, SqlType.I32) : SqlValue.Null(expression.Type);
        return name == "regexp_instr" ? SqlValue.Integer(match[(int)group * 2 + (int)endOption] + 1L, SqlType.I32) : SqlValue.Text(Slice(match[(int)group * 2], match[(int)group * 2 + 1]));
    }
    private static void Validate(string name, long value, bool valid) { if (!valid) throw new SqlException("22023", $"invalid value for parameter \"{name}\": {value}"); }
}
