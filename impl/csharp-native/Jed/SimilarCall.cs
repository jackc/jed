using System.Text;

namespace Jed;

internal sealed class SimilarCall(bool substring, SimilarPattern? pattern)
{
    internal bool Substring { get; } = substring;
    internal SimilarPattern? Pattern { get; } = pattern;
}
internal static class SimilarFunctions
{
    internal static Resolved Prepare(Resolved expression)
    {
        if (expression.Text != "substring" || expression.Args.Length != 3 || expression.Args[1].Type != SqlType.Text) return expression;
        if (expression.Args.Any(argument => argument.Op == "constant" && argument.Constant.IsNull)) return new("constant", SqlType.Text, SqlValue.Null(SqlType.Text), []);
        return expression with { Similar = Descriptor(expression.Args, true) };
    }
    internal static Resolved Resolve(Expression expression)
    {
        var args = expression.Args.Select(argument => Jed.Resolve.Expr(argument, SqlType.Text)).ToArray();
        if (args.Any(argument => argument.Type != SqlType.Text)) throw Errors.Type("SIMILAR operands require text");
        return new("similar", SqlType.Boolean, default, args, Similar: Descriptor(args, false));
    }
    private static SimilarCall Descriptor(Resolved[] arguments, bool substring)
    {
        SimilarPattern? compiled = null;
        if (arguments.Skip(1).All(argument => argument.Op == "constant" && !argument.Constant.IsNull))
            compiled = SimilarPattern.Compile(SimilarPattern.Translate(arguments[1].Constant.AsText(), arguments.Length == 3 ? arguments[2].Constant.AsText() : "\\"), substring);
        return new(substring, compiled);
    }
    internal static SqlValue Call(Resolved expression, SqlValue[] values, Meter meter)
    {
        var descriptor = expression.Similar!; var program = descriptor.Pattern;
        if (values.Skip(1).Any(value => value.IsNull)) return SqlValue.Null(expression.Type);
        if (program is null)
        {
            var translation = SimilarPattern.Translate(values[1].AsText(), values.Length == 3 ? values[2].AsText() : "\\");
            if (!descriptor.Substring) meter.Charge(Costs.OperatorEval);
            meter.Charge(TextEncoding.ByteCount(translation.Text) * Costs.ScalarByte); meter.Guard();
            if (values[0].IsNull) return SqlValue.Null(expression.Type);
            program = SimilarPattern.Compile(translation, descriptor.Substring);
        }
        if (values[0].IsNull) return SqlValue.Null(expression.Type);
        RegexMachine[] machines;
        if (descriptor.Pattern is not null)
        {
            if (!meter.SimilarMachines.TryGetValue(descriptor, out machines!))
            {
                meter.Charge(program.Count * Costs.RegexCompile); meter.Guard(); machines = program.Programs.Select(compiled => new RegexMachine(compiled)).ToArray(); meter.SimilarMachines.Add(descriptor, machines);
            }
        }
        else { meter.Charge(program.Count * Costs.RegexCompile); meter.Guard(); machines = program.Programs.Select(compiled => new RegexMachine(compiled)).ToArray(); }
        var source = values[0].AsText(); var result = program.Run(source, meter, machines);
        if (!descriptor.Substring) return SqlValue.Boolean(result.Start >= 0);
        if (result.Start < 0) return SqlValue.Null(SqlType.Text);
        var output = new StringBuilder(); var at = 0;
        foreach (var rune in source.EnumerateRunes()) { if (at >= result.End) break; if (at++ >= result.Start) output.Append(rune); }
        return SqlValue.Text(output.ToString());
    }
}
