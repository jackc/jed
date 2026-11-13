using System.Text;
using System.Text.Json;
using Jed;

internal static class RegexTests
{
    internal static void Run(string root)
    {
        var package = Path.Combine(root, "impl/csharp-native");
        using var baseline = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(package, "baseline.json")));
        using var vectors = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(package, ".build/regex-vectors.json")));
        Corpus.Equal(baseline.RootElement.GetProperty("semantic_revision").GetString(), vectors.RootElement.GetProperty("semantic_revision").GetString(), "regex fixture baseline");
        var programs = 0; var matches = 0; var similar = 0;
        foreach (var file in vectors.RootElement.GetProperty("files").EnumerateArray())
        foreach (var test in file.GetProperty("data").GetProperty("case").EnumerateArray())
        {
            if (file.GetProperty("source").GetString()!.EndsWith("similar_vectors.toml", StringComparison.Ordinal)) { Similar(test); similar++; continue; }
            var pattern = test.GetProperty("pattern").GetString()!;
            var program = RegexProgram.Compile(pattern, test.TryGetProperty("flags", out var flags) ? flags.GetString()! : "");
            if (test.TryGetProperty("prog", out var listing))
            {
                Corpus.Equal(string.Join(';', listing.EnumerateArray().Select(item => item.GetString())), string.Join(';', program.Code.Select(item => item.Listing)), $"regex program {pattern}");
                Corpus.Equal(test.GetProperty("count").GetInt32(), program.Code.Length, "regex instruction count");
                var classes = test.TryGetProperty("classes", out var items) ? string.Join(';', items.EnumerateArray().Select(item => item.GetString())) : "";
                Corpus.Equal(classes, string.Join(';', program.Classes.Select(item => item.Listing)), "regex normalized classes"); programs++;
            }
            else
            {
                var match = new RegexMachine(program).Search(test.GetProperty("input").GetString()!);
                Corpus.Equal(test.GetProperty("matched").GetBoolean(), match.Captures is not null, $"regex match {pattern}");
                Corpus.Equal(test.GetProperty("steps").GetInt64(), match.Steps, $"regex steps {pattern}");
                Corpus.Equal(string.Join(',', test.GetProperty("caps").EnumerateArray().SelectMany(pair => pair.EnumerateArray()).Select(item => item.GetInt32())), string.Join(',', match.Captures ?? []), $"regex captures {pattern}"); matches++;
            }
        }
        Console.WriteLine($"RegexTests: {programs} complete shared program vectors, {matches} complete shared capture/step vectors, {similar} complete SIMILAR translation/match/step vectors");
    }
    private static void Similar(JsonElement test)
    {
        SimilarTranslation translation;
        try { translation = SimilarPattern.Translate(test.GetProperty("pattern").GetString()!, test.TryGetProperty("escape", out var escape) ? escape.GetString()! : "\\"); }
        catch (SqlException error) when (test.TryGetProperty("error", out var state)) { Corpus.Equal(state.GetString(), error.SqlState, "SIMILAR translation error"); return; }
        if (test.TryGetProperty("error", out _)) throw new Exception("expected SIMILAR translation error");
        Corpus.Equal(test.GetProperty("translation").GetString(), translation.Text, "SIMILAR translated text");
        var input = test.GetProperty("input").GetString()!;
        var predicate = SimilarPattern.Compile(translation, false); var match = predicate.Run(input);
        Corpus.Equal(test.GetProperty("similar_compile").GetInt32(), predicate.Count, "SIMILAR compiled count");
        Corpus.Equal(test.GetProperty("similar").GetBoolean(), match.Start >= 0, "SIMILAR predicate");
        Corpus.Equal(test.GetProperty("similar_steps").GetInt64(), match.Steps, "SIMILAR predicate steps");
        SimilarPattern substring;
        try { substring = SimilarPattern.Compile(translation, true); }
        catch (SqlException error) when (test.TryGetProperty("substring_error", out var state)) { Corpus.Equal(state.GetString(), error.SqlState, "SIMILAR part error"); return; }
        if (test.TryGetProperty("substring_error", out _)) throw new Exception("expected SIMILAR part error");
        match = substring.Run(input);
        Corpus.Equal(test.GetProperty("substring_compile").GetInt32(), substring.Count, "SIMILAR substring compiled count");
        Corpus.Equal(test.GetProperty("substring_steps").GetInt64(), match.Steps, "SIMILAR substring steps");
        var actual = match.Start < 0 ? null : string.Concat(input.EnumerateRunes().Skip(match.Start).Take(match.End - match.Start));
        Corpus.Equal(test.TryGetProperty("substring", out var value) ? value.GetString() : null, actual, "SIMILAR substring");
    }

}
