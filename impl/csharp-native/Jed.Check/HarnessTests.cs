internal static class HarnessTests
{
    internal static async Task Run()
    {
        var directory = Path.Combine(Path.GetTempPath(), "jed-csharp-harness-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(directory);
        try
        {
            var file = Path.Combine(directory, "probe.test");
            foreach (var invalid in new[]
            {
                "# max_query_memory_bytes: 1\nquery I nosort\nSELECT 1\n----\n1\n",
                "# types: text\nquery I nosort\nSELECT 1\n----\n1\n",
                "# cost: 0\nquery I nosort\nSELECT 1\n----\n1\n",
                "statement error 22003\nSELECT 1 / 0\n",
                "query I nosort\nSELECT 1 / 0\n----\n1\n",
                "query I rowsort\nSELECT 1\n----\n1\n1\n",
                "# cost: 1\n",
                "skipif csharp-native\nstatement ok\nSELECT 1\n"
            })
            {
                File.WriteAllText(file, invalid);
                var failed = false;
                try { await Corpus.Run(file); }
                catch (InvalidDataException) { failed = true; }
                Corpus.Equal(true, failed, "harness rejects unsupported directives or incorrect assertions");
            }
            Console.WriteLine("PASS corpus adapter: negative checks for unsupported directives, types, costs, errors, row multiplicity and skips");
        }
        finally { Directory.Delete(directory, recursive: true); }
    }
}
