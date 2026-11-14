namespace Jed;

// A bounded specialization of the existing packed lane: direct numeric columns,
// optionally an integer-to-float cast. Folds, row order and charge boundaries
// remain the ordinary kernels; this only removes row reconstruction/dispatch.
internal sealed class PackedNumericAggregate
{
    private readonly record struct Input(int Column, SqlKind Kind, bool Count, bool Cast, bool Single);
    private readonly Input[] inputs;
    private PackedNumericAggregate(Input[] inputs) { this.inputs = inputs; }

    internal static PackedNumericAggregate? Create(SelectPlan plan, CatalogTable table)
    {
        if (plan.Filter is not null || table.Columns.Any(column => !column.Type.Type.IsInteger() && !column.Type.Type.IsFloat())) return null;
        var aggregates = plan.Group!.Aggregates;
        var inputs = new Input[aggregates.Length];
        for (var i = 0; i < inputs.Length; i++)
        {
            var aggregate = aggregates[i];
            if (aggregate.Name is not ("count" or "sum" or "avg")) return null;
            var argument = aggregate.Argument; var cast = false;
            if (argument is null)
            {
                if (aggregate.Name != "count") return null;
                inputs[i] = new(-1, SqlKind.I64, true, false, false); continue;
            }
            var resultType = argument.Type;
            if (argument.Op == "cast" && argument.Type.IsFloat() && argument.Args is [{ Op: "column" } source] && source.Type.IsInteger())
            { cast = true; argument = source; }
            if (argument.Op != "column" || argument.Slot < 0 || argument.Slot >= table.Columns.Length ||
                argument.Type != table.Columns[argument.Slot].Type.Type) return null;
            if (aggregate.Name != "count" && (resultType.IsInteger() ? aggregate.Type != SqlType.Decimal : aggregate.Type != resultType)) return null;
            inputs[i] = new(argument.Slot, argument.Type.Kind, aggregate.Name == "count", cast, resultType == SqlType.F32);
        }
        return new(inputs);
    }

    internal StorageLeaf.NumericColumn[] Scratch() => new StorageLeaf.NumericColumn[inputs.Length];

    internal void Run(StorageTree.ScanChunk chunk, StorageLeaf.NumericColumn[] columns, GroupExecution.Fold[] folds, Meter meter, CancellationToken token)
    {
        for (var i = 0; i < inputs.Length; i++)
            if (inputs[i].Column >= 0) columns[i] = new(chunk.Leaf!, inputs[i].Column);
        for (var row = chunk.Reverse ? chunk.Until - 1 : chunk.From; chunk.Reverse ? row >= chunk.From : row < chunk.Until; row += chunk.Reverse ? -1 : 1)
        {
            meter.Charge(Costs.StorageRowRead); meter.Checkpoint(token);
            for (var i = 0; i < inputs.Length; i++)
            {
                var input = inputs[i]; var fold = folds[i];
                meter.Charge(Costs.AggregateAccumulate); meter.Guard();
                if (input.Column < 0) { fold.Count = checked(fold.Count + 1); continue; }
                meter.Guard(); // Evaluate.Expr entry for the column or cast.
                if (input.Cast) { meter.Charge(Costs.OperatorEval); meter.Guard(); } // cast's child column
                if (!columns[i].Read(row, out var bits)) continue;
                fold.Count = checked(fold.Count + 1);
                if (input.Count) continue;
                if (input.Kind is SqlKind.F32 or SqlKind.F64)
                    fold.Floating!.Add(input.Kind == SqlKind.F32 ? BitConverter.UInt32BitsToSingle((uint)bits) : BitConverter.UInt64BitsToDouble(bits), input.Single);
                else
                {
                    var integer = input.Kind switch
                    {
                        SqlKind.I16 => unchecked((short)(bits ^ 0x8000)),
                        SqlKind.I32 => unchecked((int)(bits ^ 0x80000000)),
                        _ => unchecked((long)(bits ^ 0x8000000000000000))
                    };
                    if (input.Cast) fold.Floating!.Add(FloatValue.FromInteger(integer, input.Single), input.Single);
                    else fold.IntegerSum!.Add(integer, meter);
                }
            }
        }
    }
}
