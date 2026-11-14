using Jed;

internal static class PackedNumericAggregateTests
{
    internal static async Task Run()
    {
        ColumnLayouts();
        TypeSpec[] types = [new(SqlType.I32), new(SqlType.I16), new(SqlType.I64), new(SqlType.F32), new(SqlType.F64)];
        var table = new CatalogTable("numeric", types.Select((type, i) => new CatalogColumn($"c{i}", type)).ToArray(), [0]);
        var schema = table.LeafSchema();
        var aggregates = new List<AggregatePlan> { new("count", null, SqlType.I64, false, null) };
        for (var column = 0; column < types.Length; column++)
        {
            var argument = new Resolved("column", types[column].Type, default, [], Slot: column);
            foreach (var name in new[] { "count", "sum", "avg" })
                aggregates.Add(new(name, argument, name == "count" ? SqlType.I64 : argument.Type.IsInteger() ? SqlType.Decimal : argument.Type, false, null));
            if (argument.Type.IsInteger())
                foreach (var type in new[] { SqlType.F32, SqlType.F64 })
                    foreach (var name in new[] { "count", "sum", "avg" })
                        aggregates.Add(new(name, new("cast", type, default, [argument], Target: new(type)), name == "count" ? SqlType.I64 : type, false, null));
        }
        var operations = aggregates.ToArray();
        var projections = operations.Select((aggregate, i) => new Projection(new("column", aggregate.Type, default, [], Slot: i), i.ToString())).ToArray();
        foreach (var pattern in new[] { "finite", "special", "overflow", "null" })
        {
            long[] integers = [long.MinValue, long.MaxValue, 1, -1, 0, 16777217, 9007199254740993, -999999999999999];
            double[] numbers = pattern switch
            {
                "special" => [double.NaN, double.PositiveInfinity, double.NegativeInfinity, -0.0, 0, 1, -1, double.Epsilon],
                "overflow" => [double.MaxValue, double.MaxValue, -1, 0, 1, 0, 1, 0],
                _ => [1e20, 1, -1e20, -0.0, 0, 0.25, -0.125, double.Epsilon]
            };
            var records = new StorageRecord[integers.Length];
            for (var row = 0; row < records.Length; row++)
            {
                var single = pattern == "overflow" ? float.MaxValue : (float)numbers[row];
                SqlValue[] values = [SqlValue.Integer(row + 1000, SqlType.I32), SqlValue.Integer(row - 4, SqlType.I16),
                    SqlValue.Integer(integers[row]), SqlValue.Float(single, SqlType.F32), SqlValue.Float(numbers[row])];
                if (row == 4 || pattern == "null")
                    for (var column = 1; column < values.Length; column++) values[column] = SqlValue.Null(types[column].Type);
                records[row] = table.EncodeRow(values);
            }
            var encoded = StorageLeaf.Encode(8192, schema, records);
            var leaf = StorageLeaf.Parse(encoded, schema);
            for (var column = 0; column < types.Length; column++)
            {
                var view = new StorageLeaf.NumericColumn(leaf, column);
                for (var row = 0; row < records.Length; row++)
                {
                    var expected = leaf.FixedRow(row, table)[column];
                    Corpus.Equal(!expected.IsNull, view.Read(row, out var bits), "numeric column NULL bitmap");
                    if (expected.IsNull) continue;
                    var body = StoredValues.Leaf(expected, types[column])!;
                    ulong wanted = 0; foreach (var value in body) wanted = (wanted << 8) | value;
                    Corpus.Equal(wanted, bits, "numeric view matches full decoder bytes including elided keys and deltas");
                }
            }
            await using var device = new MemoryBlockStore();
            await device.SetLengthAsync(3 * 8192); await device.WriteAsync(2 * 8192, encoded);
            var pager = new StoragePager(device, 8192);
            foreach (var resident in new[] { false, true })
            foreach (var reverse in new[] { false, true })
            foreach (var empty in new[] { false, true })
            {
                var tree = new StorageTree(pager, schema, 2, 3, residentRoot: resident ? records : null);
                var scan = new TableScan(records[0].Key, true, records[^1].Key, false, reverse, Empty: empty);
                var plan = new SelectPlan(projections, null, null, 0, false, Group: new([], operations, null), AggregateLane: true,
                    Access: new(AccessKind.Primary, 7, 1, 0, 1, Scan: scan), ScanReverse: reverse);
                plan = plan with { PackedAggregate = PackedAggregateInput.Create(plan, table, tree) };
                Corpus.Equal(true, plan.PackedAggregate!.Numeric is not null, "plain numeric aggregate admits typed leaf fold");
                async Task<(string Rows, string? Error, long Cost, long Peak, long Used)> Check(int path, long ceiling, bool cancel = false, long memory = 0)
                {
                    await using var session = new Session();
                    using var cancellation = new CancellationTokenSource(); if (cancel) cancellation.Cancel();
                    var meter = new Meter(session, ceiling, cancellation.Token, new QueryOptions { MaxQueryMemoryBytes = memory });
                    var current = path == 0 ? plan with { PackedAggregate = null } : path == 1 ? plan with { PackedAggregate = plan.PackedAggregate with { Numeric = null } } : plan;
                    var rows = new List<string>(); string? error = null;
                    try
                    {
                        await foreach (var row in QueryExecution.Select(current, QueryExecution.TableSource(table, tree, meter, cancellation.Token, scan), meter, cancellationToken: cancellation.Token))
                            rows.Add(string.Join("|", row.OwnedValues.Select(value => Convert.ToHexString(StoredValues.Encode(value)))));
                    }
                    catch (SqlException exception) { error = exception.SqlState; }
                    catch (OperationCanceledException) { error = "canceled"; }
                    return (string.Join(";", rows), error, meter.Accrued, meter.Memory.Peak, meter.Memory.Used);
                }
                var expected = await Check(1, 0);
                Corpus.Equal(expected, await Check(2, 0), "typed folds preserve packed values, float ordering, overflow and accounting");
                var general = await Check(0, 0);
                Corpus.Equal((general.Rows, general.Error, general.Cost), (expected.Rows, expected.Error, expected.Cost), "typed fold matches general lane");
                var ceilings = pattern == "finite" && !resident && !reverse && !empty
                    ? Enumerable.Range(1, (int)expected.Cost + 1).Select(value => (long)value)
                    : new long[] { 1, 5, 23, 100, 1000 };
                foreach (var ceiling in ceilings)
                    Corpus.Equal(await Check(1, ceiling), await Check(2, ceiling), "every typed cost boundary preserves attempts and final failure accounting");
                Corpus.Equal(await Check(1, 0, cancel: true), await Check(2, 0, cancel: true), "typed cancellation boundary");
                foreach (var memory in new long[] { 1, 4096, 100000 })
                    Corpus.Equal(await Check(1, 0, memory: memory), await Check(2, 0, memory: memory), "memory budget retains ordinary packed admission fallback");
                Corpus.Equal(true, PackedNumericAggregate.Create(plan with { Filter = new("constant", SqlType.Boolean, SqlValue.Boolean(true), []) }, table) is null, "filtered input stays on ordinary packed lane");
                var mixed = table with { Columns = [.. table.Columns, new("other", new(SqlType.Boolean))] };
                Corpus.Equal(true, PackedNumericAggregate.Create(plan, mixed) is null, "mixed schemas retain full row validation");
            }
        }
        Console.WriteLine("PASS typed packed aggregates: numeric bodies, NULLs, order, overflow, every cost cutoff and fallback accounting");
    }

    private static void ColumnLayouts()
    {
        var table = new CatalogTable("layouts", Enumerable.Range(0, 7).Select(i => new CatalogColumn($"c{i}", new(SqlType.I64))).ToArray(), [0, 1, 2]);
        var schema = table.LeafSchema();
        // The shared prefix contains the first and second PK entirely and part
        // of the third. Other columns force integer-delta strides 0, 1, 2, 4.
        var records = Enumerable.Range(0, 20).Select(row => table.EncodeRow(new SqlValue[]
        {
            SqlValue.Integer(23), SqlValue.Integer(-9), SqlValue.Integer(0x123400 + row),
            SqlValue.Integer(100000), SqlValue.Integer(100000 + row),
            SqlValue.Integer(100000 + row * 1000), SqlValue.Integer(100000 + row * 10000000L)
        })).ToArray();
        var leaf = StorageLeaf.Parse(StorageLeaf.Encode(8192, schema, records), schema);
        for (var column = 0; column < table.Columns.Length; column++)
        {
            var view = new StorageLeaf.NumericColumn(leaf, column);
            for (var row = 0; row < records.Length; row++)
            {
                Corpus.Equal(true, view.Read(row, out var bits), "numeric composite/delta cell present");
                Corpus.Equal(leaf.FixedRow(row, table)[column].AsInteger(), unchecked((long)(bits ^ 0x8000000000000000)),
                    "composite PK prefix and all integer delta strides match full decoder");
            }
        }
    }
}
