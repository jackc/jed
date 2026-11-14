using System.Text;
using Jed;

internal static class LikeTests
{
    internal static void Run()
    {
        // The old token matcher is an independent oracle for Unicode scalar
        // matching, wildcard backtracking and encountered escape errors.
        var words = Words(["a", "b", "%", "_", "\\", "é", "😀"], 3).ToArray();
        foreach (var text in words)
            foreach (var pattern in words) Check(text, pattern);
        foreach (var text in new[] { "", "a", "ab", "ba", "baa", "a😀é%_\\b", "😀😀", "\ud83d", "\ude00", "\ud83dX\ude00", "\ufffd" })
            foreach (var pattern in new[] { "", "\\", "%\\", "a\\", "a%\\", "%a\\", "%a%b%", "%%%_%%_", "%\\%%\\_%\\\\%", "😀_", "_😀", "\ud83d", "\ude00", "%\ud83d%", "\ufffd", "\\😀%" })
                Check(text, pattern);
        Corpus.Equal((false, (string?)null), Outcome(TextFunctions.Like, "", "\\"), "empty input does not encounter trailing escape");
        Corpus.Equal((false, (string?)null), Outcome(TextFunctions.Like, "a", "b\\"), "literal mismatch does not encounter later escape");
        Corpus.Equal((false, (string?)null), Outcome(TextFunctions.Like, "a", "a\\"), "exhausted input does not encounter trailing escape");
        Corpus.Equal((false, SqlStates.InvalidEscapeSequence), Outcome(TextFunctions.Like, "ab", "a\\"), "remaining input encounters trailing escape");
        for (var i = 0; i < 32; i++) _ = TextFunctions.Like("abbaé😀%", "a%é_\\%");
        var before = GC.GetAllocatedBytesForCurrentThread();
        var matches = 0;
        for (var i = 0; i < 1000; i++) if (TextFunctions.Like("abbaé😀%", "a%é_\\%")) matches++;
        var allocated = GC.GetAllocatedBytesForCurrentThread() - before;
        Corpus.Equal(1000, matches, "rune walker matches escaped wildcard after supplementary scalar");
        Corpus.Equal(0L, allocated, "LIKE matching allocates no per-row scratch");
        Console.WriteLine("PASS LIKE walker: exhaustive token-oracle parity, Unicode, backtracking, escape error timing and zero allocations");
    }
    private static IEnumerable<string> Words(string[] alphabet, int depth)
    {
        yield return "";
        var level = new[] { "" };
        for (var length = 1; length <= depth; length++)
        {
            level = level.SelectMany(prefix => alphabet.Select(symbol => prefix + symbol)).ToArray();
            foreach (var word in level) yield return word;
        }
    }
    private static void Check(string text, string pattern) =>
        Corpus.Equal(Outcome(Reference, text, pattern), Outcome(TextFunctions.Like, text, pattern), "LIKE UTF-16 walker matches token oracle");
    private static (bool Value, string? Error) Outcome(Func<string, string, bool> matcher, string text, string pattern)
    {
        try { return (matcher(text, pattern), null); }
        catch (SqlException error) { return (false, error.SqlState); }
    }
    private static bool Reference(string text, string pattern)
    {
        var input = text.EnumerateRunes().ToArray(); var raw = pattern.EnumerateRunes().ToArray();
        var tokens = new List<(int Kind, Rune Rune)>();
        for (var i = 0; i < raw.Length; i++)
        {
            if (raw[i].Value == '\\')
            { if (++i == raw.Length) { tokens.Add((3, default)); break; } tokens.Add((0, raw[i])); }
            else tokens.Add((raw[i].Value == '%' ? 2 : raw[i].Value == '_' ? 1 : 0, raw[i]));
        }
        var at = 0; var token = 0; var star = -1; var restart = 0;
        while (at < input.Length)
        {
            if (token < tokens.Count && tokens[token].Kind == 3) throw new SqlException(SqlStates.InvalidEscapeSequence, "LIKE pattern ends in escape");
            if (token < tokens.Count && (tokens[token].Kind == 1 || tokens[token].Kind == 0 && tokens[token].Rune == input[at])) { at++; token++; }
            else if (token < tokens.Count && tokens[token].Kind == 2) { star = token++; restart = at; }
            else if (star >= 0) { token = star + 1; at = ++restart; }
            else return false;
        }
        while (token < tokens.Count && tokens[token].Kind == 2) token++;
        return token == tokens.Count;
    }
}
