using Jed;

internal static class QueryOptimizationTests
{
    internal static async Task Run()
    {
        var loads = 0;
        var resident = new DeferredSqlValue(_ => { loads++; return new(SqlValue.Integer(17)); });
        Corpus.Equal(17L, (await resident.ResolveAsync()).AsInteger(), "resident deferred value");
        Corpus.Equal(17L, (await resident.ResolveAsync()).AsInteger(), "reused resident deferred value");
        Corpus.Equal(1, loads, "resident deferred cell loads once");
        var completion = new TaskCompletionSource<SqlValue>(TaskCreationOptions.RunContinuationsAsynchronously);
        loads = 0;
        var delayed = new DeferredSqlValue(_ => { loads++; return new(completion.Task); });
        var first = delayed.ResolveAsync(); var second = delayed.ResolveAsync();
        completion.SetResult(SqlValue.Integer(23));
        Corpus.Equal(23L, (await first).AsInteger(), "first delayed consumer");
        Corpus.Equal(23L, (await second).AsInteger(), "second delayed consumer");
        Corpus.Equal(23L, (await delayed.ResolveAsync()).AsInteger(), "published delayed value");
        Corpus.Equal(1, loads, "concurrent deferred cell consumers share load");

        await using var device = new MemoryBlockStore();
        await device.SetLengthAsync(1024);
        var schema = new StorageLeafSchema([new(Width: 4, KeyOffset: 0, Integer: true)], FixedKeyWidth: 4);
        StorageRecord Record(int value)
        {
            var key = KeyEncoding.Integer(value, 4);
            return new(key, [key.ToArray()]);
        }
        var records = new[] { Record(101), Record(102), Record(103) };
        await device.WriteAsync(512, StorageLeaf.Encode(256, schema, records));
        await device.WriteAsync(768, StorageLeaf.Encode(256, schema, [Record(201)]));
        var pager = new StoragePager(device, 256, cacheBytes: 256);
        var bytes = await pager.ReadPageAsync(2);
        var leaf = pager.Leaf(2, bytes, schema);
        Corpus.Equal(true, ReferenceEquals(leaf, pager.Leaf(2, bytes, new(schema.Columns.ToArray(), 4))), "equivalent schema reuses leaf parse");
        for (var row = 0; row < records.Length; row++)
            foreach (var key in new[] { Array.Empty<byte>(), new byte[] { 0 }, new byte[] { 255 }, records[row].Key, records[row].Key[..2], records[row].Key.Concat(new byte[] { 0 }).ToArray() })
                Corpus.Equal(Math.Sign(leaf.Key(row).AsSpan().SequenceCompareTo(key)), Math.Sign(leaf.CompareKey(row, key)), "packed prefix key comparison");
        _ = await pager.ReadPageAsync(3);
        var reloadedBytes = await pager.ReadPageAsync(2);
        Corpus.Equal(false, ReferenceEquals(leaf, pager.Leaf(2, reloadedBytes, schema)), "parsed leaf evicts with page");
        pager.Invalidate(2);
        await device.WriteAsync(512, StorageLeaf.Encode(256, schema, [Record(301)]));
        var replacementBytes = await pager.ReadPageAsync(2);
        var replacement = pager.Leaf(2, replacementBytes, schema);
        _ = pager.Leaf(2, bytes, schema); // held pre-reuse bytes must not poison the new entry
        Corpus.Equal(true, ReferenceEquals(replacement, pager.Leaf(2, replacementBytes, schema)), "held old page cannot overwrite replacement parse");
        Corpus.Equal(301L, KeyEncoding.ReadInteger(replacement.Cell(0, 0)!.Value.Span), "replacement page value");
        await FixedRows();
        IntegerFloats();
        await ResidentExpressions();
        await ResidentExpressionAllocations();
        await IntegerAggregates();
        await PublicOutputIdentity();
        await AggregateLaneErrors();
        await PrimaryJoinBinding();
        IntegerKeyEncoding();
        ExecutionAllocationTests.Run();
        await PackedNumericAggregateTests.Run();
        await PackedSelectTests.Run();
        Console.WriteLine("PASS query optimization invariants: deferred publication, bounded parsed leaves, page reuse and prefix key comparisons");
    }
    private static async Task FixedRows()
    {
        TypeSpec[] types = [new(SqlType.I32), new(SqlType.I16), new(SqlType.I64), new(SqlType.Boolean),
            new(SqlType.Date), new(SqlType.Timestamp), new(SqlType.Timestamptz), new(SqlType.F32), new(SqlType.F64),
            new(SqlType.Uuid), new(SqlType.Interval), new(SqlType.Decimal, 4, 2), new(SqlType.Decimal, 9, 2), new(SqlType.Decimal, 18, 2)];
        var table = new CatalogTable("fixed", types.Select((type, i) => new CatalogColumn($"c{i}", type)).ToArray(), [0, 1]);
        var schema = table.LeafSchema();
        var records = new StorageRecord[40];
        for (var i = 0; i < records.Length; i++)
        {
            SqlValue[] values = [SqlValue.Integer(123456, SqlType.I32), SqlValue.Integer(i, SqlType.I16), SqlValue.Integer(99999999 + i),
                SqlValue.Boolean(i % 2 == 0), SqlValue.Date(i - 20), SqlValue.Timestamp(i - 20), SqlValue.Timestamp(i - 20, true),
                SqlValue.Float(i * .25, SqlType.F32), SqlValue.Float(i * -.125), SqlValue.OwnedBytes(Enumerable.Repeat((byte)i, 16).ToArray(), SqlType.Uuid),
                SqlValue.Interval(new(i, -i, i * 100000L)), SqlValue.Decimal(ExactDecimal.Parse("12.34")),
                SqlValue.Decimal(ExactDecimal.Parse("12345.67")), SqlValue.Decimal(ExactDecimal.Parse("1234567890.12"))];
            if (i % 3 == 0) for (var c = 2; c < values.Length; c++) values[c] = SqlValue.Null(types[c].Type);
            var cells = values.Select((value, c) => StoredValues.Leaf(value, types[c])).ToArray();
            records[i] = new([.. cells[0]!, .. cells[1]!], cells);
        }
        var encoded = StorageLeaf.Encode(8192, schema, records);
        var leaf = StorageLeaf.Parse(encoded, schema);
        for (var i = 0; i < records.Length; i++)
        {
            var direct = leaf.FixedRow(i, table);
            for (var c = 0; c < types.Length; c++)
            {
                var copied = StoredValues.FromLeaf(leaf.Cell(i, c), types[c]);
                Corpus.Equal(true, StoredValues.Encode(copied).AsSpan().SequenceEqual(StoredValues.Encode(direct[c])), "packed fixed row matches owned cell decoding");
            }
        }
        await using var device = new MemoryBlockStore();
        await device.SetLengthAsync(3 * 8192);
        await device.WriteAsync(2 * 8192, encoded);
        var pager = new StoragePager(device, 8192);
        foreach (var resident in new StorageRecord[]?[] { null, records })
        {
            var tree = new StorageTree(pager, schema, 2, 3, residentRoot: resident);
            foreach (var target in new[] { 20, 99 })
            {
                byte[] key = [.. KeyEncoding.Integer(123456, 4), .. KeyEncoding.Integer(target, 2)];
                var count = 0;
                await foreach (var row in tree.ScanRangeRowsAsync(key, true, key, true))
                { Corpus.Equal((long)target, row.DecodeFixed(table, tree)[1].AsInteger(), "exact point row view"); count++; }
                Corpus.Equal(target == 20 ? 1 : 0, count, "present and absent point cardinality");
            }
            await PackedAggregates(table, tree, records[10].Key, records[29].Key);
            foreach (var reverse in new[] { false, true })
            {
                var expected = reverse ? 28 : 11; var count = 0;
                await foreach (var row in tree.ScanRangeRowsAsync(records[10].Key, false, records[29].Key, false, reverse))
                {
                    Corpus.Equal((long)expected, row.DecodeFixed(table, tree)[1].AsInteger(), "fixed row scan bounds and direction");
                    expected += reverse ? -1 : 1; count++;
                }
                Corpus.Equal(18, count, "fixed row scan cardinality");
            }
        }
    }

    private static async Task PackedAggregates(CatalogTable table, StorageTree tree, byte[] lower, byte[] upper)
    {
        var value = new Resolved("column", SqlType.I64, default, [], Slot: 2);
        var cast = new Resolved("cast", SqlType.F64, default, [value], Target: new(SqlType.F64));
        AggregatePlan[] aggregates = [new("count", null, SqlType.I64, false, null), new("sum", value, SqlType.Decimal, false, null),
            new("avg", value, SqlType.Decimal, false, null), new("sum", cast, SqlType.F64, false, null),
            new("min", value, SqlType.I64, false, null), new("max", value, SqlType.I64, false, null)];
        var projections = aggregates.Select((aggregate, index) => new Projection(new("column", aggregate.Type, default, [], Slot: index), index.ToString())).ToArray();
        var key = new Resolved("column", SqlType.I16, default, [], Slot: 1);
        var failure = new Resolved("/", SqlType.I16, default, [new("constant", SqlType.I16, SqlValue.Integer(1, SqlType.I16), []),
            new("-", SqlType.I16, default, [key, new("constant", SqlType.I16, SqlValue.Integer(20, SqlType.I16), [])])]);
        Resolved[] filters = [new(">", SqlType.Boolean, default, [key, new("constant", SqlType.I16, SqlValue.Integer(15, SqlType.I16), [])]),
            new(">", SqlType.Boolean, default, [failure, new("constant", SqlType.I16, SqlValue.Integer(0, SqlType.I16), [])])];
        foreach (var reverse in new[] { false, true })
        foreach (var filter in filters)
        foreach (var ceiling in new long[] { 0, 1, 17, 68, 10000 })
        {
            var scan = new TableScan(lower, false, upper, false, reverse);
            var plan = new SelectPlan(projections, filter, null, 0, false, Group: new([], aggregates, null), AggregateLane: true,
                Access: new(AccessKind.Primary, 18, 1, 0, 1, Scan: scan), ScanReverse: reverse);
            plan = plan with { PackedAggregate = PackedAggregateInput.Create(plan, table, tree) };
            Corpus.Equal(true, plan.PackedAggregate is not null, "fixed numeric whole aggregate admits packed input");
            async Task<(string Rows, string? Error, long Cost)> Run(bool packed)
            {
                await using var session = new Session();
                var meter = new Meter(session, ceiling, default);
                var rows = new List<string>(); string? error = null;
                var current = packed ? plan : plan with { PackedAggregate = null };
                try
                {
                    await foreach (var row in QueryExecution.Select(current, QueryExecution.TableSource(table, tree, meter, default, scan), meter))
                        rows.Add(string.Join("|", row.OwnedValues.Select(cell => Convert.ToHexString(StoredValues.Encode(cell)))));
                }
                catch (SqlException exception) { error = exception.SqlState; }
                return (string.Join(";", rows), error, meter.Accrued);
            }
            Corpus.Equal(await Run(false), await Run(true), "packed page fold matches generic lane values, errors and cost");
        }
    }

    private static void IntegerFloats()
    {
        void Check(long value)
        {
            var text = value.ToString(System.Globalization.CultureInfo.InvariantCulture);
            foreach (var single in new[] { false, true })
                Corpus.Equal(BitConverter.DoubleToUInt64Bits(FloatValue.Parse(text, single)),
                    BitConverter.DoubleToUInt64Bits(FloatValue.FromInteger(value, single)), "direct integer IEEE conversion matches decimal parse");
        }
        Check(0); Check(1); Check(-1); Check(long.MinValue); Check(long.MaxValue);
        foreach (var exponent in new[] { 15, 24, 25, 31, 32, 52, 53, 54, 62 })
        {
            var center = 1L << exponent;
            for (var delta = -4; delta <= 4; delta++) { Check(center + delta); Check(-center + delta); }
            // Binary32 half-way integers, including their immediate neighbors,
            // reveal accidental int64 -> binary64 -> binary32 double rounding.
            if (exponent >= 24)
            {
                var half = 1L << (exponent - 24);
                for (var delta = -2; delta <= 2; delta++)
                { Check(center + half + delta); Check(-center - half + delta); }
            }
        }
        ulong random = 0x123456789abcdef0UL;
        for (var i = 0; i < 10000; i++)
        {
            random ^= random << 13; random ^= random >> 7; random ^= random << 17;
            Check(unchecked((long)random));
        }
    }

    private static async Task ResidentExpressionAllocations()
    {
        await using var session = new Session();
        var meter = new Meter(session, 0, default);
        SqlValue[] row = [SqlValue.Integer(12345)];
        var column = new Resolved("column", SqlType.I64, default, [], Slot: 0);
        var cast = new Resolved("cast", SqlType.F64, default, [column], Target: new(SqlType.F64));
        foreach (var expression in new[] { column, cast })
        {
            SqlValue value = default;
            for (var warmup = 0; warmup < 32; warmup++) value = Evaluate.Expr(expression, meter, row: row);
            var before = GC.GetAllocatedBytesForCurrentThread();
            for (var repeat = 0; repeat < 4096; repeat++) value = Evaluate.Expr(expression, meter, row: row);
            var allocated = GC.GetAllocatedBytesForCurrentThread() - before;
            Corpus.Equal(0L, allocated, "resident scalar leaf/cast allocates no captured argument evaluator");
            Corpus.Equal("12345", value.ToString(), "allocation probe consumes scalar result");
        }
        foreach (var left in new[] { "", "a", "a🙂é", "a🙃e\u0301" })
        foreach (var right in new[] { "", "a", "a🙂é", "a🙃e\u0301" })
        {
            var cost = meter.Accrued;
            var compared = Evaluate.Compare(SqlValue.Text(left), SqlValue.Text(right), meter);
            Corpus.Equal(Math.Sign(TextEncoding.Utf8.GetBytes(left).AsSpan().SequenceCompareTo(TextEncoding.Utf8.GetBytes(right))),
                Math.Sign(compared), "text scalar order matches UTF8 byte order");
            Corpus.Equal(Math.Max(0, Math.Min(left.EnumerateRunes().Count(), right.EnumerateRunes().Count()) - 1) * Costs.VarlenCompare,
                meter.Accrued - cost, "text comparison meters Unicode scalars rather than UTF16 code units");
        }
        var a = SqlValue.Text("a🙂é"); var b = SqlValue.Text("a🙃e\u0301");
        for (var warmup = 0; warmup < 32; warmup++) _ = Evaluate.Compare(a, b, meter);
        var textBefore = GC.GetAllocatedBytesForCurrentThread();
        for (var repeat = 0; repeat < 4096; repeat++) _ = Evaluate.Compare(a, b, meter);
        Corpus.Equal(0L, GC.GetAllocatedBytesForCurrentThread() - textBefore, "metered text comparison does not box rune enumerators");
    }

    private static async Task ResidentExpressions()
    {
        var calls = 0;
        var function = new HostFunction("once", [SqlType.I64], SqlType.I64, arguments => { calls++; return arguments[0]; });
        Resolved Column(int slot, SqlType type) => new("column", type, default, [], Slot: slot);
        Resolved Constant(SqlValue value) => new("constant", value.Type, value, []);
        var host = new Resolved("host", SqlType.I64, default, [Column(1, SqlType.I64)], HostFunction: function);
        var failure = new Resolved("/", SqlType.I64, default, [Constant(SqlValue.Integer(1)), Constant(SqlValue.Integer(0))]);
        var choice = new Resolved("case", SqlType.I64, default, [Column(0, SqlType.Boolean), host, failure]);
        var cast = new Resolved("cast", SqlType.F64, default,
            [new("+", SqlType.I64, default, [Column(1, SqlType.I64), Constant(SqlValue.Integer(3))])]);
        var shortCircuit = new Resolved("and", SqlType.Boolean, default,
            [Column(0, SqlType.Boolean), new("=", SqlType.Boolean, default, [host, Constant(SqlValue.Integer(17))])]);
        async Task<(string? Value, string? Error, long Cost, int Calls)> Run(Resolved expression, bool deferred, SqlValue flag, long ceiling, bool filter)
        {
            await using var session = new Session(); var meter = new Meter(session, ceiling, default);
            calls = 0;
            SqlValue[] row = [flag, SqlValue.Integer(17)];
            if (deferred)
                for (var i = 0; i < row.Length; i++)
                {
                    var value = row[i];
                    row[i] = SqlValue.Deferred(value.Type, new DeferredSqlValue(async _ => { await Task.Yield(); return value; }));
                }
            string? result = null, error = null;
            try { result = (await Evaluate.ExprAsync(expression, meter, filter, row)).ToString(); }
            catch (SqlException failure) { error = failure.SqlState; }
            return (result, error, meter.Accrued, calls);
        }
        foreach (var expression in new[] { cast, choice, shortCircuit })
            foreach (var flag in new[] { SqlValue.Boolean(true), SqlValue.Boolean(false), SqlValue.Null(SqlType.Boolean) })
                foreach (var filter in new[] { false, true })
                    foreach (var ceiling in new[] { 0L, 1L, 2L, 3L, 4L, 5L, 6L })
                        Corpus.Equal(await Run(expression, false, flag, ceiling, filter), await Run(expression, true, flag, ceiling, filter),
                            "resident and suspended expression values, errors, costs and host call count");
        var selected = await Run(choice, false, SqlValue.Boolean(true), 0, false);
        Corpus.Equal(1, selected.Calls, "selected resident host call executes once");
        var skipped = await Run(shortCircuit, true, SqlValue.Boolean(false), 0, false);
        Corpus.Equal(0, skipped.Calls, "suspended short circuit skips host call");
    }

    private static async Task IntegerAggregates()
    {
        var magnitudes = new List<UInt128> { 0, (UInt128)long.MaxValue, (UInt128)long.MaxValue + 1,
            ulong.MaxValue, (UInt128)ulong.MaxValue + 1, (UInt128)Int128.MaxValue, (UInt128)Int128.MaxValue + 1, UInt128.MaxValue };
        UInt128 threshold = 1;
        for (var group = 0; group < 10; group++)
        {
            magnitudes.Add(threshold - 1); magnitudes.Add(threshold); magnitudes.Add(threshold + 1);
            if (group < 9) threshold *= 10000;
        }
        foreach (var magnitude in magnitudes)
        {
            var digits = magnitude.ToString(System.Globalization.CultureInfo.InvariantCulture).Length;
            Corpus.Equal(magnitude == 0 ? 0 : (digits + 3) / 4, AggregateIntegerSum.CountGroups(magnitude),
                "integer aggregate groups at every decimal threshold and machine-width extrema");
        }
        long?[] inputs = [null, 0, 9999, 1, -10000, 99999999, 1, -100000000,
            long.MaxValue, long.MaxValue, long.MinValue, long.MinValue, 2,
            -9999, -1, 10000, -99999999, -1, 100000000, null];
        foreach (var name in new[] { "sum", "avg" })
            foreach (var spillAt in new[] { -1, 9, 13 })
            {
                var plan = new AggregatePlan(name, new("column", SqlType.I64, default, [], Slot: 0), SqlType.Decimal, false, null);
                var direct = new SpillFold(plan); var reference = new SpillFold(plan);
                reference.Finite.Sum = ExactDecimal.Zero; // force arbitrary-precision reference fold
                await using var directSession = new Session(); await using var referenceSession = new Session();
                var directMeter = new Meter(directSession, 0, default); var referenceMeter = new Meter(referenceSession, 0, default);
                for (var i = 0; i < inputs.Length; i++)
                {
                    var value = inputs[i] is { } input ? SqlValue.Integer(input) : SqlValue.Null(SqlType.I64);
                    GroupExecution.Accumulate(plan, direct.Finite, value, directMeter);
                    GroupExecution.Accumulate(plan, reference.Finite, value, referenceMeter);
                    Corpus.Equal(reference.Finite.Sum.ToString(), direct.Finite.Sum.ToString(), "wide integer fold equals decimal after each signed input");
                    Corpus.Equal(reference.LogicalBytes(), direct.LogicalBytes(), "integer fold logical bytes at base10000 boundaries");
                    Corpus.Equal(referenceMeter.Accrued, directMeter.Accrued, "integer fold exact decimal work");
                    if (i == spillAt)
                    {
                        var writer = new WireWriter(); direct.Write(writer);
                        var restored = new SpillFold(plan);
                        ReadFold(restored, writer.ToArray()); direct = restored;
                    }
                }
                Corpus.Equal(GroupExecution.Finalize(plan, reference.Finite).ToString(), GroupExecution.Finalize(plan, direct.Finite).ToString(),
                    "integer SUM/AVG finalization across scratch restore");
            }
        static void ReadFold(SpillFold fold, byte[] bytes)
        { var reader = new WireReader(bytes); fold.Read(ref reader); reader.Finish(); }
    }

    private static async Task PrimaryJoinBinding()
    {
        await using var database = await Database.CreateInMemoryAsync();
        await using var pin = await database.PinAsync();
        var storage = new QueryStorage(database, pin.Snapshot, null, pin.Snapshot.Catalog);
        foreach (var values in new SqlValue[][] {
            [SqlValue.Integer(-17, SqlType.I32), SqlValue.Integer(23, SqlType.I32), SqlValue.Integer(99, SqlType.I32)],
            [SqlValue.Text("a\0é"), SqlValue.Text("🙂"), SqlValue.Text("missing")],
            [SqlValue.Float(-0.0), SqlValue.Float(double.NaN), SqlValue.Float(17.5)] })
        {
            var type = values[0].Type;
            var table = new CatalogTable("probe", [new("id", new(type), true)], [0], RowCount: 2);
            await using var device = new MemoryBlockStore();
            var tree = new StorageTree(new StoragePager(device, 512), table.LeafSchema());
            foreach (var value in values.Take(2)) tree = await tree.UpsertAsync(table.EncodeRow([value]));
            var source = new BaseRelationAccess(table, tree, null, storage);
            var outer = new Resolved("column", type, default, [], Slot: 0);
            var inner = new Resolved("column", type, default, [], Slot: 1);
            foreach (var reverse in new[] { false, true })
            {
                var bound = new Resolved("=", SqlType.Boolean, default, reverse ? [inner, outer] : [outer, inner]);
                var join = new IndexNestedLoop(source, [bound], 1) { PinnedAccess = new(AccessKind.Primary, 1, 1, 0, 1) };
                foreach (var value in values.Append(SqlValue.Null(type)))
                foreach (var ceiling in new[] { 0L, 1L, 2L, 3L })
                foreach (var cancelled in new[] { false, true })
                {
                    async Task<(string Rows, string? Error, long Cost, long Peak, long Used, int Loads)> Run(bool fast)
                    {
                        using var cancellation = new CancellationTokenSource();
                        if (cancelled) cancellation.Cancel();
                        await using var session = new Session();
                        var meter = new Meter(session, ceiling, cancellation.Token, new() { MaxQueryMemoryBytes = 64 });
                        var loads = 0;
                        var deferred = SqlValue.Deferred(type, new DeferredSqlValue(async _ => { loads++; await Task.Yield(); return value; }));
                        var rows = new List<string>(); string? error = null;
                        try
                        {
                            var input = fast ? join.Rows([deferred], meter, cancellation.Token) : join.ReadGeneric([deferred], meter, cancellation.Token);
                            await foreach (var row in input)
                                rows.Add(Convert.ToHexString(StoredValues.Encode(await row[0].ResolveAsync(cancellation.Token))));
                        }
                        catch (SqlException failure) { error = failure.SqlState; }
                        catch (OperationCanceledException) { error = "cancelled"; }
                        return (string.Join(';', rows), error, meter.Accrued, meter.Memory.Peak, meter.Memory.Used, loads);
                    }
                    Corpus.Equal(await Run(false), await Run(true), "primary join binder preserves deferred reads, rows, cost, memory, failures and cancellation");
                }
            }
        }
    }

    private static void IntegerKeyEncoding()
    {
        foreach (var type in new[] { SqlType.I16, SqlType.I32, SqlType.I64 })
        {
            var fact = SqlTypes.Fact(type);
            foreach (var number in new[] { fact.Min, fact.Min + 1, -1, 0, 1, fact.Max - 1, fact.Max })
            foreach (var nullable in new[] { false, true })
            foreach (var descending in new[] { false, true })
            {
                var value = SqlValue.Integer(number, type);
                var reference = new WireWriter();
                if (nullable) reference.Byte(0);
                reference.Bytes(KeyEncoding.Integer(number, StoredValues.ScalarWidth(type)));
                var expected = reference.ToArray();
                if (descending) for (var index = 0; index < expected.Length; index++) expected[index] ^= 255;
                var encoded = KeyEncoding.Encode(value, nullable, descending);
                Corpus.Equal(Convert.ToHexString(expected), Convert.ToHexString(encoded), "integer key bytes preserve width, sign, prefix and direction");
                encoded[0] ^= 255;
                Corpus.Equal(Convert.ToHexString(expected), Convert.ToHexString(KeyEncoding.Encode(value, nullable, descending)), "each encoded integer key owns independent bytes");
            }
            Corpus.Equal("01", Convert.ToHexString(KeyEncoding.Encode(SqlValue.Null(type), nullable: true)), "nullable integer key marker");
            Corpus.Equal("FE", Convert.ToHexString(KeyEncoding.Encode(SqlValue.Null(type), nullable: true, descending: true)), "descending NULL key marker");
            string? error = null;
            try { _ = KeyEncoding.Encode(SqlValue.Null(type)); } catch (SqlException failure) { error = failure.SqlState; }
            Corpus.Equal(SqlStates.NotNullViolation, error, "nonnullable integer NULL key error");
        }
    }

    private static async Task PublicOutputIdentity()
    {
        static async IAsyncEnumerable<SqlValue[]> Values(long value)
        { await ValueTask.CompletedTask; yield return [SqlValue.Integer(value)]; }
        var original = Values(1);
        var direct = QueryExecution.Materialized([new Row([SqlValue.Integer(1)])]);
        var relation = new PreparedRelation([], original) { PublicSource = original, PublicOutput = direct };
        Corpus.Equal(true, ReferenceEquals(direct, relation.OutputRows()), "unchanged source uses direct public output");
        var copied = relation with { Columns = [] };
        Corpus.Equal(true, ReferenceEquals(direct, copied.OutputRows()), "metadata-only relation copy keeps output identity");
        var transformed = relation with { Rows = Values(2) };
        Corpus.Equal(false, ReferenceEquals(direct, transformed.OutputRows()), "row transformation invalidates direct output");
        await foreach (var row in transformed.OutputRows())
            Corpus.Equal(2L, row[0].AsInteger(), "transformed relation emits transformed values");
    }

    private static async Task AggregateLaneErrors()
    {
        var calls = 0;
        var environment = DatabaseEnvironment.Builder().RegisterFunction(new("lane_once", [SqlType.I32], SqlType.I32,
            values => { calls++; return values[0]; })).Build();
        await using var database = await Database.CreateInMemoryAsync(environment: environment);
        await using var session = database.CreateSession();
        await session.ExecuteAsync("CREATE TABLE lane_cost(id i32 PRIMARY KEY)");
        await session.ExecuteAsync("INSERT INTO lane_cost VALUES (1),(2),(3),(4),(5),(6)");
        async Task<(string State, long Cost)> Failure(string operand, long ceiling, bool host = false)
        {
            var before = session.LifetimeCost;
            var filter = host ? "lane_once(id)" : "id";
            try
            {
                await session.ExecuteAsync($"SELECT sum({operand}) FROM lane_cost WHERE 1000 / (3 - {filter}) > 0", options: new() { MaxCost = ceiling });
                throw new InvalidDataException("aggregate filter unexpectedly succeeded");
            }
            catch (SqlException error) { return (error.SqlState, session.LifetimeCost - before); }
        }
        for (var ceiling = 0; ceiling <= 22; ceiling++)
        {
            var eager = await Failure("DISTINCT id", ceiling);
            Corpus.Equal(eager, await Failure("id", ceiling), "bare lane filter error and ceiling settle to eager cost");
            Corpus.Equal(eager, await Failure("id::f64", ceiling), "expression lane rejects attempt with eager error and cost");
            if (ceiling == 0) Corpus.Equal(("22012", 16L), eager, "independent Go failed-cost fixture: whole bound7 plus filter9");
        }
        calls = 0;
        Corpus.Equal(("22012", 19L), await Failure("id", 0, true), "volatile filter settles scan without replay");
        Corpus.Equal(3, calls, "volatile filter executes each visited row once on error");
    }

}
