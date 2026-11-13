using System.Text.Json;
using Jed;

internal static class SimulationImageDump
{
    internal static async Task Run(string path, int key)
    {
        using var credential = key < 0 ? null : DatabaseCredential.FromKey(Enumerable.Repeat((byte)(0x4a + key), 32).ToArray());
        var bytes = await File.ReadAllBytesAsync(path);
        await using var database = credential is null
            ? await Database.OpenStoreAsync(new MemoryBlockStore(bytes))
            : await Database.OpenEncryptedStoreAsync(new MemoryBlockStore(bytes), credential.Value, count => new byte[count]);
        await using var pin = await database.PinAsync();
        var snapshot = pin.Snapshot; var state = database.CommittedStorageForTesting();
        if (Environment.GetEnvironmentVariable("JED_SIM_PLAIN_OUT") is { Length: > 0 } decodedPath)
        {
            var decoded = bytes.ToArray(); var size = snapshot.Meta.PageSize;
            var codec = snapshot.Pager!.Codec;
            for (uint page = codec is null ? 2u : 4u; page < snapshot.Meta.PageCount; page++)
            {
                var offset = checked((int)page * size);
                if (codec is null) continue;
                try
                {
                    var body = codec.OpenBody(page, bytes.AsSpan(offset, size), bytes.AsSpan(offset + size - 16, 16));
                    body.CopyTo(decoded, offset);
                }
                catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted)
                { decoded.AsSpan(offset, size).Clear(); } // manifest/free/stale bytes are not current authenticated bodies
            }
            await File.WriteAllBytesAsync(decodedPath, decoded);
            var records = new List<object>();
            foreach (var table in snapshot.Catalog.Tables)
                await foreach (var row in database.Tree(table, null, snapshot).ScanAsync())
                    records.Add(new { table = table.Name, key = Convert.ToHexString(row.Key),
                        cells = row.Cells.Select(cell => cell is null ? null : new { length = cell.Length,
                            tag = cell.Length == 0 ? -1 : cell[0], bytes = Convert.ToHexString(cell) }).ToArray() });
            await File.WriteAllTextAsync(decodedPath + ".rows.json", JsonSerializer.Serialize(records));
        }
        object? Shape(StorageTreeShape? shape) => shape is null ? null : new
        {
            page = shape.Page, separators = shape.Separators.Select(Convert.ToHexString).ToArray(),
            children = shape.Children.Select(Shape).ToArray()
        };
        Console.WriteLine(JsonSerializer.Serialize(new { version = database.Version, pages = snapshot.Meta.PageCount,
            live = snapshot.Meta.LivePages, free = state.FreePages.ToArray(), catalogPages = snapshot.Catalog.Pages,
            tables = snapshot.Catalog.Tables.Select(table => new { table.Name, table.RowCount, shape = Shape(table.Shape),
                indexes = (table.Indexes ?? []).Select(index => new { index.Name, shape = Shape(index.Shape) }) }) }));
    }
}
