namespace Jed;

// GiST bounds use the decodable range body or fixed-width scalar key brackets.
// Tuple bounds concatenate components; the catalog supplies each component type.
internal sealed record GistComponent(SqlValue? Range, byte[] Lower, byte[] Upper);
internal sealed record GistBounds(GistComponent[] Components)
{
    internal static void ValidateType(SqlType type)
    {
        if (type.Kind is SqlKind.Range or SqlKind.Boolean or SqlKind.I16 or SqlKind.I32 or SqlKind.I64 or SqlKind.Uuid or SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz) return;
        if (type.Kind is SqlKind.Text or SqlKind.Bytea or SqlKind.Decimal or SqlKind.Interval) throw Errors.Unsupported("GiST scalar type is not supported");
        throw new SqlException(SqlStates.UndefinedObject, $"no GiST operator class for {type.Name()}");
    }
    internal static GistBounds? FromValues(SqlValue[] values)
    {
        if (values.Any(value => value.IsNull)) return null;
        return new(values.Select(value => value.Type.Kind == SqlKind.Range ? new GistComponent(value, [], []) : Scalar(value)).ToArray());
    }
    private static GistComponent Scalar(SqlValue value)
    {
        var key = KeyEncoding.Encode(value); return new(null, key, key);
    }
    internal byte[] Encode()
    {
        var writer = new WireWriter();
        foreach (var component in Components)
        {
            if (component.Range is { } range) OpenCodec.Write(writer, range, 0);
            else { writer.U16(checked((ushort)component.Lower.Length)); writer.Bytes(component.Lower); writer.U16(checked((ushort)component.Upper.Length)); writer.Bytes(component.Upper); }
        }
        return writer.ToArray();
    }
    internal static GistBounds Decode(ReadOnlySpan<byte> bytes, SqlType[] types)
    {
        var reader = new WireReader(bytes); var components = new GistComponent[types.Length];
        for (var i = 0; i < types.Length; i++)
        {
            if (types[i].Kind == SqlKind.Range) components[i] = new(OpenCodec.Read(ref reader, types[i], 0), [], []);
            else
            {
                var lower = reader.Bytes(reader.U16()).ToArray(); var upper = reader.Bytes(reader.U16()).ToArray();
                var width = StoredValues.Width(new(types[i]));
                if (lower.Length != width || upper.Length != width || lower.AsSpan().SequenceCompareTo(upper) > 0) throw StorageFormat.Corrupt("invalid GiST scalar bracket");
                components[i] = new(null, lower, upper);
            }
        }
        reader.Finish(); return new(components);
    }
    internal GistBounds Union(GistBounds other)
    {
        var result = new GistComponent[Components.Length];
        for (var i = 0; i < result.Length; i++)
        {
            var a = Components[i]; var b = other.Components[i];
            result[i] = a.Range is { } range ? new(OpenFunctions.RangeSet("merge", range, b.Range!.Value), [], []) :
                new(null, Compare(a.Lower, b.Lower) <= 0 ? a.Lower : b.Lower, Compare(a.Upper, b.Upper) >= 0 ? a.Upper : b.Upper);
        }
        return new(result);
    }
    internal int CompareTo(GistBounds other)
    {
        for (var i = 0; i < Components.Length; i++)
        {
            var a = Components[i]; var b = other.Components[i];
            var order = a.Range is { } range ? Evaluate.Compare(range, b.Range!.Value) : Compare(a.Lower, b.Lower);
            if (order == 0 && a.Range is null) order = Compare(a.Upper, b.Upper);
            if (order != 0) return order;
        }
        return 0;
    }
    // Strategies: 0 overlap, 1 equality, 2 contains (range or element).
    internal bool Matches(SqlValue[] queries, byte[] strategies, bool leaf)
    {
        for (var i = 0; i < Components.Length; i++)
        {
            var query = queries[i]; if (query.IsNull) return false;
            var bound = Components[i];
            if (bound.Range is { } range)
            {
                if (strategies[i] == 1 && leaf)
                { if (Evaluate.Compare(range, query) != 0) return false; }
                else if (strategies[i] == 1 && query.AsRange().IsEmpty) { /* empty leaves may lie below any hull */ }
                else if (!OpenFunctions.Predicate(strategies[i] == 2 && query.Type.Kind != SqlKind.Range ? "@>" : "&&", range, query).AsBoolean()) return false;
            }
            else
            {
                var key = KeyEncoding.Encode(query);
                if (Compare(key, bound.Lower) < 0 || Compare(key, bound.Upper) > 0) return false;
            }
        }
        return true;
    }
    private static int Compare(byte[] a, byte[] b) => a.AsSpan().SequenceCompareTo(b);
}
