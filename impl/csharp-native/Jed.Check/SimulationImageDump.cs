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
        if (System.Environment.GetEnvironmentVariable("JED_SIM_PLAIN_OUT") is { Length: > 0 } decodedPath)
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
            using var records = new MemoryStream();
            using (var writer = new Utf8JsonWriter(records))
            {
                writer.WriteStartArray();
                foreach (var table in snapshot.Catalog.Tables)
                    await foreach (var row in database.Tree(table, null, snapshot).ScanAsync())
                    {
                        writer.WriteStartObject(); writer.WriteString("table", table.Name); writer.WriteString("key", Convert.ToHexString(row.Key));
                        writer.WriteStartArray("cells");
                        foreach (var cell in row.Cells)
                        {
                            if (cell is null) { writer.WriteNullValue(); continue; }
                            writer.WriteStartObject(); writer.WriteNumber("length", cell.Length); writer.WriteNumber("tag", cell.Length == 0 ? -1 : cell[0]);
                            writer.WriteString("bytes", Convert.ToHexString(cell)); writer.WriteEndObject();
                        }
                        writer.WriteEndArray(); writer.WriteEndObject();
                    }
                writer.WriteEndArray(); writer.Flush();
            }
            await File.WriteAllBytesAsync(decodedPath + ".rows.json", records.ToArray());
        }
        static void Shape(Utf8JsonWriter writer, StorageTreeShape? shape)
        {
            if (shape is null) { writer.WriteNullValue(); return; }
            writer.WriteStartObject(); writer.WriteNumber("page", shape.Page); writer.WriteStartArray("separators");
            foreach (var separator in shape.Separators) writer.WriteStringValue(Convert.ToHexString(separator));
            writer.WriteEndArray(); writer.WriteStartArray("children");
            foreach (var child in shape.Children) Shape(writer, child);
            writer.WriteEndArray(); writer.WriteEndObject();
        }
        using var output = new MemoryStream();
        using (var writer = new Utf8JsonWriter(output))
        {
            writer.WriteStartObject(); writer.WriteNumber("version", database.Version); writer.WriteNumber("pages", snapshot.Meta.PageCount);
            writer.WriteNumber("live", snapshot.Meta.LivePages); writer.WriteStartArray("free");
            foreach (var page in state.FreePages) writer.WriteNumberValue(page);
            writer.WriteEndArray(); writer.WriteStartArray("catalogPages");
            foreach (var page in snapshot.Catalog.Pages) writer.WriteNumberValue(page);
            writer.WriteEndArray(); writer.WriteStartArray("tables");
            foreach (var table in snapshot.Catalog.Tables)
            {
                writer.WriteStartObject(); writer.WriteString("Name", table.Name); writer.WriteNumber("RowCount", table.RowCount);
                writer.WritePropertyName("shape"); Shape(writer, table.Shape); writer.WriteStartArray("indexes");
                foreach (var index in table.Indexes ?? [])
                {
                    writer.WriteStartObject(); writer.WriteString("Name", index.Name); writer.WritePropertyName("shape"); Shape(writer, index.Shape); writer.WriteEndObject();
                }
                writer.WriteEndArray(); writer.WriteEndObject();
            }
            writer.WriteEndArray(); writer.WriteEndObject(); writer.Flush();
        }
        Console.WriteLine(System.Text.Encoding.UTF8.GetString(output.ToArray()));
    }
}
