namespace Jed;

internal sealed record HashJoinPlan(int[] Left, int[] Right, SqlType[] Types)
{
    internal static HashJoinPlan? Select(Resolved? predicate, int leftWidth, string kind, long? leftRows, long? rightRows, Resolved? residual)
    {
        if (predicate is null || kind is not ("inner" or "left")) return null;
        var left = new List<int>(); var right = new List<int>(); var types = new List<SqlType>();
        static bool KeyType(SqlType type) => type.Kind switch
        {
            SqlKind.Json or SqlKind.Jsonb or SqlKind.Jsonpath or SqlKind.Composite or SqlKind.Unknown => false,
            SqlKind.Array or SqlKind.Range => type.Element!.Kind is not (SqlKind.Array or SqlKind.Range or SqlKind.Composite) && KeyType(type.Element!),
            _ => true
        };
        foreach (var term in PredicatePlan.Conjuncts(predicate))
        {
            if (term.Op is not ("=" or "<>" or "!=") || !PredicatePlan.Safe(term) || term.Args.Any(arg => arg.Op is not ("column" or "constant"))) return null;
            if (term.Op != "=" || term.Args[0].Op != "column" || term.Args[1].Op != "column") continue;
            var a = term.Args[0]; var b = term.Args[1];
            if (a.Slot >= leftWidth) (a, b) = (b, a);
            if (a.Slot >= leftWidth || b.Slot < leftWidth || a.Type != b.Type || !KeyType(a.Type)) continue;
            left.Add(a.Slot); right.Add(b.Slot - leftWidth); types.Add(a.Type);
        }
        if (left.Count == 0) return null;
        return new(left.ToArray(), right.ToArray(), types.ToArray());
    }
    internal static long Work(long a, long b, SqlType[] types, Resolved? residual, long minimumCandidates = 0)
    {
        var pairs = Estimate.Multiply(a, b); var candidates = Math.Max(minimumCandidates, Estimate.Rows(pairs, residual));
        var nodes = residual is null ? 0 : PredicatePlan.Nodes(residual); long width = 0;
        foreach (var type in types) width = Estimate.Add(width, type.Kind switch
        {
            SqlKind.Boolean => 1, SqlKind.I16 => 2, SqlKind.I32 or SqlKind.F32 or SqlKind.Date => 4,
            SqlKind.I64 or SqlKind.F64 or SqlKind.Timestamp or SqlKind.Timestamptz => 8,
            SqlKind.Uuid or SqlKind.Interval => 16, _ => PlannerFacts.VariableKeyBytes
        });
        return Estimate.Add(Estimate.Multiply(Estimate.Add(a, b), width), Estimate.Multiply(candidates, Estimate.Add(nodes, Estimate.Add(4L * types.Length, width))));
    }
}

internal sealed class HashJoinTable(Meter meter) : IDisposable
{
    private long heldBytes;
    public void Dispose() { meter.Memory.Release(heldBytes); heldBytes = 0; }
    private sealed record Entry(byte[] Key, int Position);
    private readonly Dictionary<ulong, List<Entry>> buckets = [];
    internal static async ValueTask<HashJoinTable> Build(HashJoinPlan plan, IReadOnlyList<SqlValue[]> rows, Meter meter)
    {
        var table = new HashJoinTable(meter);
        try
        {
        for (var i = 0; i < rows.Count; i++)
        {
            var key = await Key(rows[i], plan.Right, Costs.HashBuild, meter).ConfigureAwait(false);
            if (key is null) continue;
            if (meter.Memory.Enabled)
            {
                var values = new SqlValue[plan.Right.Length];
                for (var slot = 0; slot < values.Length; slot++) values[slot] = await rows[i][plan.Right[slot]].ResolveAsync(meter.Cancellation).ConfigureAwait(false);
                var bytes = MemorySize.EntryBytes(values); meter.Memory.Reserve(bytes); table.heldBytes = Meter.Add(table.heldBytes, bytes);
            }
            var hash = Hash(key);
            if (!table.buckets.TryGetValue(hash, out var bucket)) { bucket = []; table.buckets.Add(hash, bucket); }
            bucket.Add(new(key, i));
        }
        return table;
        }
        catch { table.Dispose(); throw; }
    }
    internal async ValueTask<int[]> Probe(HashJoinPlan plan, SqlValue[] row, Meter meter)
    {
        var key = await Key(row, plan.Left, Costs.HashProbe, meter).ConfigureAwait(false);
        if (key is null || !buckets.TryGetValue(Hash(key), out var bucket)) return [];
        var matches = new List<int>();
        foreach (var entry in bucket)
        {
            meter.Charge(Math.Max(1, Math.Min(key.Length, entry.Key.Length)) * Costs.HashProbe); meter.Guard();
            if (key.AsSpan().SequenceEqual(entry.Key)) matches.Add(entry.Position);
        }
        return matches.ToArray();
    }
    internal static async ValueTask<byte[]?> Key(SqlValue[] row, int[] slots, long unit, Meter meter)
    {
        var writer = new WireWriter(); var present = true;
        foreach (var slot in slots)
        {
            var value = await row[slot].ResolveAsync(meter.Cancellation).ConfigureAwait(false);
            var bytes = value.IsNull ? [] : KeyEncoding.Encode(value);
            meter.Charge(Math.Max(1, bytes.Length) * unit); meter.Guard();
            present &= !value.IsNull;
            var length = (uint)bytes.Length;
            writer.Byte((byte)(length >> 24)); writer.Byte((byte)(length >> 16)); writer.Byte((byte)(length >> 8)); writer.Byte((byte)length); writer.Bytes(bytes);
        }
        return present ? writer.ToArray() : null;
    }
    internal static ulong Hash(ReadOnlySpan<byte> bytes)
    {
        var value = 14695981039346656037UL;
        foreach (var item in bytes) value = unchecked((value ^ item) * 1099511628211UL);
        return value;
    }
}
