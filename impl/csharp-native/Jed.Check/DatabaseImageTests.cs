using Jed;

internal static class DatabaseImageTests
{
    internal static async Task Run(string root)
    {
        string[] names = ["empty_db", "pk_table", "text_table", "tall_tree", "multiway_split", "max_sep_table",
            "compact_leaf_table", "overflow_table", "compressed_table", "index_table", "ordered_index_table",
            "partial_index_table", "unique_table", "array_table", "composite_type_table", "range_table",
            "json_table", "jsonb_table", "sequence_table", "statistics_table", "view_table", "view_options_table",
            "gin_array_table", "gin_multi_table", "gin_jsonb_table", "gist_range_table", "gist_scalar_table", "gist_exclude_table"];
        foreach (var name in names)
        {
            var bytes = await File.ReadAllBytesAsync(Path.Combine(root, "spec/fileformat/fixtures", name + ".jed"));
            await using var database = await Database.DeserializeAsync(bytes);
            var copied = await database.SerializeAsync();
            Corpus.Equal(true, bytes.AsSpan().SequenceEqual(copied), "complete from-scratch image " + name);
            bytes.AsSpan().Fill(0);
            var again = await database.SerializeAsync();
            Corpus.Equal(true, copied.AsSpan().SequenceEqual(again), "deserialize owns input " + name);
        }
        await using (var database = await Database.CreateInMemoryAsync(256))
        {
            await using var session = database.CreateSession();
            await DatabaseFileTests.Exec(session, "CREATE TABLE t(id i32 PRIMARY KEY, value text)");
            await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES (1,'first')");
            await DatabaseFileTests.Exec(session, "BEGIN");
            await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES (2,'pending')");
            var image = await database.SerializeAsync();
            await using var imported = await Database.DeserializeAsync(image);
            await using var read = imported.CreateSession();
            Corpus.Equal("1", await DatabaseFileTests.Scalar(read, "SELECT count(*) FROM t"), "export excludes pending transaction");
            await DatabaseFileTests.Exec(session, "ROLLBACK");
            await DatabaseFileTests.Exec(read, "INSERT INTO t VALUES (2,'independent')");
            Corpus.Equal("1", await DatabaseFileTests.Scalar(session, "SELECT count(*) FROM t"), "import commits are independent");
            await HostTests.Error(SqlStates.UndefinedObject, async () => { _ = await database.SerializeAsync("temp"); });
        }
        var encrypted = await File.ReadAllBytesAsync(Path.Combine(root, "spec/fileformat/fixtures/encrypted_empty_db.jed"));
        await HostTests.Error(SqlStates.ObjectNotInPrerequisiteState, async () => { await using var invalid = await Database.DeserializeAsync(encrypted); });
        await HostTests.Error(SqlStates.DataCorrupted, async () => { await using var invalid = await Database.DeserializeAsync(new byte[12]); });
        Console.WriteLine($"PASS database images: {names.Length} complete byte-exact shared images, input ownership, committed-only export, independent imported commits and invalid/encrypted image rejection");
    }
}
