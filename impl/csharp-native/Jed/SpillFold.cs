namespace Jed;

// A group's finite state is evicted independently of its growing inputs.
internal sealed class SpillFold(AggregatePlan plan)
{
    internal readonly GroupExecution.Fold Finite = new(plan) { StorageManaged = true };
    internal long Items;
    internal bool JsonSeen;
    internal byte[] ArrayShape = [];
    internal long LogicalBytes()
    {
        long fields;
        if (plan.WithinOrder is not null || plan.Order is { Length: > 0 }) fields = 0;
        else if (Finite.Moments is { } moments) fields = moments.LogicalBytes(plan);
        else if (plan.Name == "count") fields = MemorySchedule.Value;
        else if (Finite.Floating is not null) fields = 5 * MemorySchedule.Value;
        else if (plan.Name is "sum" or "avg") fields = MemorySize.ValueBytes(plan.Type == SqlType.Decimal ? SqlValue.StoredDecimal(Finite.Sum) : Finite.Value) + MemorySchedule.Value;
        else if (AggregateJson.IsJson(plan.Name) || plan.Name is "array_agg" or "string_agg")
            fields = MemorySchedule.Value + (plan.Name == "array_agg" && plan.Argument!.Type.Kind == SqlKind.Array ? MemorySchedule.Value * (1 + 2 * (ArrayShape.Length == 0 ? 0 : ArrayShape[0])) : 0);
        else fields = MemorySize.ValueBytes(Finite.Value);
        return Meter.Add(MemorySchedule.Value, fields); // composite(packed)
    }
    internal void Write(WireWriter writer)
    {
        writer.U64((ulong)Finite.Count); ScratchValues.Write(writer, Finite.Value);
        ScratchValues.Write(writer, SqlValue.StoredDecimal(Finite.Sum));
        Finite.Moments?.Write(writer); Finite.Floating?.Write(writer);
        writer.U64((ulong)Items); writer.Byte(JsonSeen ? (byte)1 : (byte)0);
        writer.U32((uint)ArrayShape.Length); writer.Bytes(ArrayShape);
    }
    internal void Read(ref WireReader reader)
    {
        Finite.Count = checked((long)reader.U64()); Finite.Value = ScratchValues.Read(ref reader, plan.Type);
        Finite.Sum = ScratchValues.Read(ref reader, SqlType.Decimal).AsDecimal();
        Finite.Moments?.Read(ref reader); Finite.Floating?.Read(ref reader);
        Items = checked((long)reader.U64()); JsonSeen = reader.Byte() != 0;
        ArrayShape = reader.Bytes(checked((int)reader.U32())).ToArray();
    }
}
