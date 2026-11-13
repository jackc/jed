using System.Text.Json;
using Jed;

internal static class OpenStorageTests
{
    internal static async Task Run(string root)
    {
        using var baseline = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(root, "impl/csharp-native/baseline.json")));
        var revision = baseline.RootElement.GetProperty("semantic_revision").GetString();
        var directory = Path.Combine(root, $"impl/csharp-native/.build/baseline-{revision}/spec/fileformat/fixtures");
        var names = new[] { "array_table", "array_pk_table", "array_composite_table", "composite_type_table", "nested_composite_table", "composite_array_field_table", "composite_range_field_table", "composite_pk_table", "composite_key_table", "range_table", "range_pk_table", "gin_array_table", "gist_range_table", "collation_array_table", "json_table", "jsonb_table" };
        var rows = 0; var cells = 0;
        foreach (var name in names)
        {
            await using var device = new FileBlockStore(Path.Combine(directory, name + ".jed"), FileMode.Open, readOnly: true);
            var recovered = await StorageRecovery.OpenAsync(device); var pager = new StoragePager(device, recovered.Meta.PageSize);
            var catalog = await Catalog.ReadAsync(pager, recovered.Meta);
            foreach (var table in catalog.Tables)
            {
                var tree = new StorageTree(pager, table.LeafSchema(), table.Root, recovered.Meta.PageCount);
                await foreach (var record in tree.ScanAsync())
                {
                    var values = table.DecodeRow(record);
                    for (var i = 0; i < values.Length; i++)
                    {
                        var rewritten = StoredValues.Leaf(values[i], table.Columns[i].Type);
                        Corpus.Equal(Convert.ToHexString(record.Cells[i] ?? []), Convert.ToHexString(rewritten ?? []), $"{name} {table.Name}.{table.Columns[i].Name} exact cell bytes");
                        var value = values[i];
                        if (!value.IsNull && value.Type.Kind is SqlKind.Array or SqlKind.Composite or SqlKind.Range)
                        {
                            var parsed = OpenInput.Parse(value.ToString(), value.Type);
                            Corpus.Equal(0, OpenValue.Compare(value, parsed), $"{name} recursive input/output total order");
                        }
                        cells++;
                    }
                    if (table.PrimaryKey.Length != 0 && table.PrimaryKey.All(index => table.Columns[index].Collation is null))
                    {
                        var rewritten = table.EncodeRow(values);
                        Corpus.Equal(Convert.ToHexString(record.Key), Convert.ToHexString(rewritten.Key), $"{name} exact row key bytes");
                    }
                    rows++;
                }
            }
        }
        Console.WriteLine($"PASS open value cells: {names.Length} shared golden files, {rows} rows, {cells} byte-exact cells and recursive text inverses; uncollated primary keys checked, collated primary key bytes pending");
    }
}
