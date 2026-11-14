using Jed;

internal static class ExecutionAllocationTests
{
    internal static void Run()
    {
        Keys();
        IntegerKeys();
        FiniteFolds();
        _ = Ranking(64, false);
        var allocated = Ranking(4096, false);
        Corpus.Equal(true, allocated < 4096L * 224 + 16384, "ranking avoids unused operand arrays and per-row empty key arrays");
        _ = Ranking(128, true);
        Console.WriteLine($"PASS runtime identity ownership and ranking scratch: {allocated} bytes for 4096 rows");
    }

    private static void FiniteFolds()
    {
        var plan = new AggregatePlan("count", null, SqlType.I64, false, null);
        var folds = new GroupExecution.Fold[4096];
        _ = new GroupExecution.Fold(plan);
        var before = GC.GetAllocatedBytesForCurrentThread();
        for (var index = 0; index < folds.Length; index++) folds[index] = new(plan) { Count = index };
        var allocated = GC.GetAllocatedBytesForCurrentThread() - before;
        Corpus.Equal(true, allocated < folds.Length * 256L, "finite folds do not allocate unused collection, text or DISTINCT buffers");
        Corpus.Equal(4095L, GroupExecution.Finalize(plan, folds[^1]).AsInteger(), "finite fold allocation probe retains usable state");
    }

    private static void Keys()
    {
        SqlValue[][] equal = [
            [SqlValue.Null(SqlType.I64), SqlValue.Null(SqlType.Text)],
            [SqlValue.Decimal(ExactDecimal.Parse("1.00")), SqlValue.Decimal(ExactDecimal.Parse("1"))],
            [SqlValue.Float(-0.0), SqlValue.Float(0.0)],
            [SqlValue.Float(double.NaN), SqlValue.Float(BitConverter.Int64BitsToDouble(unchecked((long)0x7ff8000000000001UL)))],
            [SqlValue.Jsonb(JsonInput.Parse("{\"a\":1,\"b\":2}")), SqlValue.Jsonb(JsonInput.Parse("{\"b\":2,\"a\":1.0}"))],
            [SqlValue.Array(new(SqlType.I64, [SqlValue.Integer(1), SqlValue.Null(SqlType.I64)])),
             SqlValue.Array(new(SqlType.I64, [SqlValue.Integer(1), SqlValue.Null(SqlType.I64)]))]
        ];
        foreach (var pair in equal)
        {
            var key = GroupExecution.EncodedIdentity([pair[0]]);
            var retained = new HashSet<byte[]>(ByteKeyComparer.Instance) { key };
            Corpus.Equal(true, retained.Contains(GroupExecution.EncodedIdentity([pair[1]])), "byte identities preserve SQL grouping equivalence");
            var duplicate = GroupExecution.EncodedIdentity([pair[0]]);
            duplicate[0] ^= 255;
            Corpus.Equal(true, retained.Contains(GroupExecution.EncodedIdentity([pair[0]])), "each nonempty identity owns its bytes");
        }
        SqlValue[] tuple = [SqlValue.Text("a\0🙂"), SqlValue.Integer(-17, SqlType.I32), SqlValue.Bytes([0, 255])];
        var framed = new WireWriter();
        foreach (var value in tuple)
        {
            var encoded = KeyEncoding.Encode(value, nullable: true);
            framed.U32((uint)encoded.Length); framed.Bytes(encoded);
        }
        var original = GroupExecution.EncodedIdentity(tuple);
        Corpus.Equal(Convert.ToHexString(original), Convert.ToHexString(GroupExecution.EncodedIdentity(tuple[0], tuple[1..])), "DISTINCT first/rest encoding preserves tuple framing");
        Corpus.Equal(Convert.ToHexString(framed.ToArray()), Convert.ToHexString(original), "identity keeps exact length-prefixed key framing");
        var entries = new Dictionary<byte[], int>(ByteKeyComparer.Instance) { [original] = 7 };
        tuple[0] = SqlValue.Text("changed");
        Corpus.Equal(false, entries.ContainsKey(GroupExecution.EncodedIdentity(tuple)), "retained identity does not alias input row storage");
        Corpus.Equal(7, entries[framed.ToArray()], "structural key lookup preserves independent owned keys");
        var shifted = SqlValue.Array(new(SqlType.I64, [SqlValue.Integer(1), SqlValue.Null(SqlType.I64)], [2], [0]));
        Corpus.Equal(false, ByteKeyComparer.Instance.Equals(GroupExecution.EncodedIdentity([equal[^1][0]]), GroupExecution.EncodedIdentity([shifted])), "array lower bounds remain part of identity");
    }

    private static void IntegerKeys()
    {
        SqlValue[] values = [SqlValue.Integer(short.MinValue, SqlType.I16), SqlValue.Integer(short.MaxValue, SqlType.I16),
            SqlValue.Integer(int.MinValue, SqlType.I32), SqlValue.Integer(int.MaxValue, SqlType.I32),
            SqlValue.Integer(long.MinValue), SqlValue.Integer(long.MaxValue), SqlValue.Integer(0), SqlValue.Integer(-1),
            SqlValue.Null(SqlType.I64), SqlValue.Null(SqlType.Text), default];
        var expected = new WireWriter();
        foreach (var value in values)
        {
            var width = value.IsNull ? 0 : StoredValues.ScalarWidth(value.Type);
            expected.U32((uint)(width + 1)); expected.Byte(value.IsNull ? (byte)1 : (byte)0);
            if (width != 0) expected.Bytes(KeyEncoding.Integer(value.AsInteger(), width));
        }
        Corpus.Equal(Convert.ToHexString(expected.ToArray()), Convert.ToHexString(GroupExecution.EncodedIdentity(values)), "integer tuple identities retain signed widths and NULL framing");
        Corpus.Equal(Convert.ToHexString(expected.ToArray()), Convert.ToHexString(GroupExecution.EncodedIdentity(values[0], values[1..])), "integer first/rest identity matches full tuple");
        SqlValue[] pair = [SqlValue.Integer(17), SqlValue.Null(SqlType.I32)];
        _ = GroupExecution.EncodedIdentity(pair);
        var before = GC.GetAllocatedBytesForCurrentThread();
        var key = GroupExecution.EncodedIdentity(pair);
        var allocated = GC.GetAllocatedBytesForCurrentThread() - before;
        Corpus.Equal(true, allocated <= 48, "integer identity allocates only its 18-byte owned result");
        pair[0] = SqlValue.Integer(18);
        Corpus.Equal(false, ByteKeyComparer.Instance.Equals(key, GroupExecution.EncodedIdentity(pair)), "integer identity never aliases mutable input tuple");
    }

    private static long Ranking(int count, bool memory)
    {
        static async IAsyncEnumerable<SqlValue[]> Source(int count)
        {
            await ValueTask.CompletedTask;
            for (var index = 0; index < count; index++) yield return [SqlValue.Integer(index)];
        }
        var stage = new WindowStage(1, [new("row_number", [], SqlType.I64, [], [], null)]);
        var session = new Session();
        try
        {
            var meter = new Meter(session, 0, default, new() { MaxQueryMemoryBytes = memory ? 1000000 : 0 });
            var before = GC.GetAllocatedBytesForCurrentThread();
            var input = WindowExecution.Run(stage, Source(count), null, meter, default).GetAsyncEnumerator();
            var produced = 0; long total = 0;
            try
            {
                while (true)
                {
                    var step = input.MoveNextAsync();
                    if (!step.IsCompleted) throw new InvalidOperationException("resident ranking unexpectedly suspended");
                    if (!step.GetAwaiter().GetResult()) break;
                    produced++; total += input.Current[1].AsInteger();
                    if (memory) meter.Memory.Release(MemorySize.RowBytes(input.Current));
                }
            }
            finally { input.DisposeAsync().GetAwaiter().GetResult(); }
            var allocated = GC.GetAllocatedBytesForCurrentThread() - before;
            Corpus.Equal(count, produced, "ranking emits every input row");
            Corpus.Equal((long)count * (count + 1) / 2, total, "ranking keeps row-number values");
            Corpus.Equal(count * Costs.WindowResult, meter.Accrued, "ranking retains exact logical cost");
            if (memory)
            {
                var rowBytes = MemorySize.RowBytes([SqlValue.Integer(0), SqlValue.Integer(1)]);
                Corpus.Equal(count * rowBytes + MemorySchedule.Entry, meter.Memory.Peak, "ranking scratch cleanup preserves logical peak");
                Corpus.Equal(0L, meter.Memory.Used, "ranking transfers output ownership without leaks");
                Corpus.Equal(0, meter.Memory.ReleaseUnderflows, "ranking account releases exactly once");
            }
            return allocated;
        }
        finally { session.DisposeAsync().GetAwaiter().GetResult(); }
    }
}
