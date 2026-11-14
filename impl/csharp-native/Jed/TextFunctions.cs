using System.Text;

namespace Jed;

internal static class TextFunctions
{
    private static string Join(IEnumerable<Rune> runes) => string.Concat(runes.Select(r => r.ToString()));
    internal static SqlValue Call(string name, SqlValue[] args, Meter meter)
    {
        var text = args[0].AsText();
        var runes = text.EnumerateRunes().ToArray();
        string result;
        switch (name)
        {
            case "length": case "char_length": case "character_length": return SqlValue.Integer(runes.Length, SqlType.I32);
            case "octet_length": return SqlValue.Integer(args[0].TextBytes, SqlType.I32);
            case "bit_length": return SqlValue.Integer(args[0].TextBytes * 8L, SqlType.I32);
            case "ascii": return SqlValue.Integer(runes.Length == 0 ? 0 : runes[0].Value, SqlType.I32);
            case "starts_with": return SqlValue.Boolean(text.StartsWith(args[1].AsText(), StringComparison.Ordinal));
            case "strpos": case "position":
                var index = text.IndexOf(args[1].AsText(), StringComparison.Ordinal);
                return SqlValue.Integer(index < 0 ? 0 : text[..index].EnumerateRunes().Count() + 1, SqlType.I32);
            case "substr": case "substring":
            {
                var start = args[1].AsInteger(); var count = args.Length > 2 ? args[2].AsInteger() : long.MaxValue;
                if (count < 0) throw new SqlException(SqlStates.SubstringError, "negative substring length");
                var first = Math.Clamp(start <= 0 ? 0 : start - 1, 0, runes.Length);
                var end = args.Length == 2 ? runes.Length : start > long.MaxValue - count ? runes.Length : Math.Clamp(start + count <= 0 ? 0 : start + count - 1, 0, runes.Length);
                result = Join(runes.Skip((int)first).Take((int)Math.Max(0, end - first))); break;
            }
            case "overlay":
            {
                var start = args[2].AsInteger();
                var replacement = args[1].AsText();
                var count = args.Length == 4 ? args[3].AsInteger() : replacement.EnumerateRunes().Count();
                if (start <= 0) throw new SqlException(SqlStates.SubstringError, "overlay start must be positive");
                if (start > int.MaxValue || count < int.MinValue || count > int.MaxValue || start + count > int.MaxValue || start + count < int.MinValue) throw Errors.Overflow();
                var prefix = (int)Math.Min(runes.Length, start - 1);
                var suffix = (int)Math.Clamp(start + count - 1, 0, runes.Length);
                result = Join(runes.Take(prefix)) + replacement + Join(runes.Skip(suffix)); break;
            }
            case "left": case "right":
            {
                var count = args[1].AsInteger();
                var take = (int)(count < 0 ? Math.Max(0, runes.Length + count) : Math.Min(runes.Length, count));
                result = Join(name == "left" ? runes.Take(take) : runes.Skip(runes.Length - take)); break;
            }
            case "reverse": result = Join(runes.Reverse()); break;
            case "lower": result = UnicodeData.Fold(text, false, meter.Unicode); break;
            case "upper": result = UnicodeData.Fold(text, true, meter.Unicode); break;
            case "initcap":
            {
                var word = false; var builder = new StringBuilder();
                foreach (var c in text)
                {
                    var letter = c is >= 'a' and <= 'z' or >= 'A' and <= 'Z';
                    builder.Append(letter ? word ? c is >= 'A' and <= 'Z' ? (char)(c + 32) : c : c is >= 'a' and <= 'z' ? (char)(c - 32) : c : c);
                    word = letter || c is >= '0' and <= '9';
                }
                result = builder.ToString(); break;
            }
            case "btrim": case "ltrim": case "rtrim":
            {
                var set = (args.Length == 1 ? " " : args[1].AsText()).EnumerateRunes().ToHashSet();
                var first = 0; var end = runes.Length;
                if (name != "rtrim") while (first < end && set.Contains(runes[first])) first++;
                if (name != "ltrim") while (end > first && set.Contains(runes[end - 1])) end--;
                result = Join(runes.Skip(first).Take(end - first)); break;
            }
            case "translate":
            {
                var from = args[1].AsText().EnumerateRunes().ToArray(); var to = args[2].AsText().EnumerateRunes().ToArray();
                var builder = new StringBuilder();
                foreach (var r in runes)
                {
                    var at = Array.IndexOf(from, r);
                    if (at < 0) builder.Append(r.ToString()); else if (at < to.Length) builder.Append(to[at].ToString());
                }
                result = builder.ToString(); break;
            }
            case "split_part":
            {
                var delimiter = args[1].AsText(); var field = args[2].AsInteger();
                if (field == 0) throw new SqlException(SqlStates.InvalidParameterValue, "field position must not be zero");
                var fields = delimiter.Length == 0 ? [text] : text.Split(delimiter, StringSplitOptions.None);
                var at = field < 0 ? fields.LongLength + field : field - 1;
                result = at < 0 || at >= fields.Length ? "" : fields[(int)at]; break;
            }
            case "repeat":
            {
                var count = args[1].AsInteger();
                if (count <= 0 || text.Length == 0) return SqlValue.Text("");
                if (count > 0x3fffffff / (long)args[0].TextBytes) throw new SqlException(SqlStates.ProgramLimitExceeded, "repeat output too large");
                meter.Charge(args[0].TextBytes); meter.Guard(); meter.ScalarOutput(count * args[0].TextBytes);
                var builder = new StringBuilder(checked((int)count * text.Length));
                for (var i = 0L; i < count; i++) builder.Append(text);
                result = builder.ToString(); break;
            }
            case "lpad": case "rpad":
            {
                var count = args[1].AsInteger(); var fill = args.Length == 3 ? args[2].AsText() : " ";
                if (count <= 0) return SqlValue.Text("");
                if (count > 0x3fffffff) throw new SqlException(SqlStates.ProgramLimitExceeded, "padding length too large");
                meter.Charge(args[0].TextBytes + (long)TextEncoding.ByteCount(fill)); meter.Guard();
                var fillRunes = fill.EnumerateRunes().ToArray();
                var kept = (int)Math.Min(runes.Length, count); var missing = fillRunes.Length == 0 ? 0 : count - kept;
                var bytes = runes.Take(kept).Sum(r => (long)r.Utf8SequenceLength);
                if (missing > 0) bytes += missing / fillRunes.Length * TextEncoding.ByteCount(fill) + fillRunes.Take((int)(missing % fillRunes.Length)).Sum(r => (long)r.Utf8SequenceLength);
                meter.ScalarOutput(bytes);
                var builder = new StringBuilder();
                if (name == "rpad") builder.Append(Join(runes.Take(kept)));
                for (var i = 0L; i < missing; i++) builder.Append(fillRunes[(int)(i % fillRunes.Length)].ToString());
                if (name == "lpad") builder.Append(Join(runes.Take(kept)));
                result = builder.ToString(); break;
            }
            case "replace":
            {
                var from = args[1].AsText(); var to = args[2].AsText();
                meter.Charge(args[0].TextBytes); meter.Guard();
                long matches = 0;
                if (from.Length > 0)
                    for (var at = 0; at <= text.Length - from.Length;)
                    { var found = text.IndexOf(from, at, StringComparison.Ordinal); if (found < 0) break; matches++; at = found + from.Length; }
                meter.ScalarOutput(args[0].TextBytes + matches * (args[2].TextBytes - (long)args[1].TextBytes));
                result = from.Length == 0 ? text : text.Replace(from, to, StringComparison.Ordinal); break;
            }
            case "like_escape":
            {
                var escapes = args[1].AsText().EnumerateRunes().ToArray();
                if (escapes.Length > 1) throw new SqlException(SqlStates.InvalidEscapeSequence, "escape must be empty or one character");
                if (escapes.Length == 1 && escapes[0].Value == '\\') { result = text; break; }
                var builder = new StringBuilder();
                for (var i = 0; i < runes.Length; i++)
                {
                    if (escapes.Length == 1 && runes[i] == escapes[0])
                    { builder.Append('\\'); if (++i == runes.Length) break; builder.Append(runes[i].ToString()); }
                    else { if (runes[i].Value == '\\') builder.Append('\\'); builder.Append(runes[i].ToString()); }
                }
                result = builder.ToString(); break;
            }
            case "quote_ident": result = TextFormat.Identifier(text); break;
            case "quote_literal": case "quote_nullable":
                result = (text.Contains('\\') ? "E" : "") + "'" + text.Replace("\\", "\\\\", StringComparison.Ordinal).Replace("'", "''", StringComparison.Ordinal) + "'"; break;
            default: throw Errors.Unsupported($"text function {name}");
        }
        return SqlValue.Text(result);
    }

    internal static bool Like(string text, string pattern)
    {
        var at = 0; var token = 0; var star = -1; var restart = 0;
        while (at < text.Length)
        {
            var kind = -1; var literal = 0; var after = token;
            if (token < pattern.Length)
            {
                var current = pattern[token]; after = token + 1;
                if (current == '\\')
                {
                    // Match the existing encountered-error rule: a trailing
                    // escape fails only while an input scalar still remains.
                    if (after == pattern.Length) throw new SqlException(SqlStates.InvalidEscapeSequence, "LIKE pattern ends in escape");
                    literal = LikeRune(pattern, after, out after); kind = 0;
                }
                else if (current == '%') kind = 2;
                else if (current == '_') kind = 1;
                else { literal = LikeRune(pattern, token, out after); kind = 0; }
            }
            var input = LikeRune(text, at, out var next);
            if (kind == 1 || kind == 0 && literal == input)
            {
                at = next; token = after;
            }
            else if (kind == 2) { star = after; token = after; restart = at; }
            else if (star >= 0)
            {
                token = star; _ = LikeRune(text, restart, out restart); at = restart;
            }
            else return false;
        }
        while (token < pattern.Length && pattern[token] == '%') token++;
        return token == pattern.Length;
    }

    // EnumerateRunes substitutes U+FFFD for an unpaired surrogate. Preserve that
    // behavior without constructing rune arrays or allocating an enumerator.
    private static int LikeRune(string value, int index, out int next)
    {
        var first = value[index]; next = index + 1;
        if (!char.IsSurrogate(first)) return first;
        if (char.IsHighSurrogate(first) && next < value.Length && char.IsLowSurrogate(value[next]))
            return 0x10000 + ((first - 0xd800) << 10) + value[next++] - 0xdc00;
        return 0xfffd;
    }
}
