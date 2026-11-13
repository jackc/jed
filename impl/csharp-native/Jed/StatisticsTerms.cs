namespace Jed;

// Shared GIN term bytes, also used by ANALYZE without requiring an index.
internal static class StatisticsTerms
{
    internal static bool Eligible(SqlType type) => type.Kind == SqlKind.Jsonb || type.Kind == SqlKind.Array && type.Element!.Kind is
        SqlKind.Boolean or SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.I16 or SqlKind.I32 or SqlKind.I64 or
        SqlKind.F32 or SqlKind.F64 or SqlKind.Decimal or SqlKind.Text or SqlKind.Bytea or SqlKind.Uuid or SqlKind.Interval;
    internal static IEnumerable<byte[]> Extract(SqlValue value)
    {
        var terms = new SortedSet<byte[]>(StatisticsBytes.Instance);
        if (value.IsNull) return terms;
        if (value.Type.Kind == SqlKind.Jsonb) Json(value.AsJsonb(), true, terms);
        else foreach (var element in value.AsArray().Values)
        {
            if (element.IsNull) continue;
            byte[] term;
            if (element.Type == SqlType.Text) term = Escape(Prefix(TextEncoding.Utf8.GetBytes(element.AsText())));
            else if (element.Type == SqlType.Bytea) term = Escape(Prefix(element.ByteSpan));
            else { term = KeyEncoding.Encode(element); if (element.Type == SqlType.Decimal) term = Prefix(term).ToArray(); }
            terms.Add(term);
        }
        return terms;
    }
    internal static byte[] Scalar(SqlJsonNode scalar, bool arrayElement = true)
    {
        var terms = new SortedSet<byte[]>(StatisticsBytes.Instance); Json(scalar, arrayElement, terms);
        if (terms.Count != 1) throw new ArgumentException("GIN scalar term requires one scalar");
        return terms.Min!;
    }
    private static ReadOnlySpan<byte> Prefix(ReadOnlySpan<byte> bytes) => bytes[..Math.Min(bytes.Length, StatisticsLimits.MaxValueBytes)];
    private static byte[] Escape(ReadOnlySpan<byte> bytes)
    {
        var writer = new WireWriter();
        foreach (var value in bytes) { writer.Byte(value); if (value == 0) writer.Byte(255); }
        writer.Byte(0); writer.Byte(1); return writer.ToArray();
    }
    private static void StringTerm(byte flag, ReadOnlySpan<byte> bytes, SortedSet<byte[]> terms)
    {
        var writer = new WireWriter(); writer.Byte((byte)(flag | (bytes.Length > StatisticsLimits.MaxValueBytes ? 16 : 0)));
        writer.Bytes(Escape(Prefix(bytes))); terms.Add(writer.ToArray());
    }
    private static void Json(SqlJsonNode node, bool arrayElement, SortedSet<byte[]> terms)
    {
        switch (node.Kind)
        {
            case SqlJsonKind.Null: terms.Add([2]); break;
            case SqlJsonKind.False: terms.Add([3, 0]); break;
            case SqlJsonKind.True: terms.Add([3, 1]); break;
            case SqlJsonKind.Number: StringTerm(4, KeyEncoding.Encode(SqlValue.StoredDecimal(node.Number!)), terms); break;
            case SqlJsonKind.String: StringTerm(arrayElement ? (byte)1 : (byte)5, TextEncoding.Utf8.GetBytes(node.Text!), terms); break;
            case SqlJsonKind.Array: foreach (var element in node.Elements) Json(element, true, terms); break;
            case SqlJsonKind.Object:
                foreach (var member in node.Members) { StringTerm(1, TextEncoding.Utf8.GetBytes(member.Key), terms); Json(member.Value, false, terms); }
                break;
        }
    }
}

internal sealed class StatisticsBytes : IComparer<byte[]>
{
    internal static readonly StatisticsBytes Instance = new();
    public int Compare(byte[]? left, byte[]? right) => left.AsSpan().SequenceCompareTo(right);
}
