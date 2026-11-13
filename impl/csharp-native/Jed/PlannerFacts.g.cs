// Generated data from b1a6affca7725e0b07a4f734eddb1c569ef76bcf.
// Regenerate only with mise run csharp-native:codegen.
namespace Jed;

internal static class PlannerFacts
{
    internal const long VariableKeyBytes = 1;
    internal const long DistinctValues = 200;
    internal const long SrfRows = 1000;
    internal const long ArrayElements = 10;
    internal static readonly (string Name, long Numerator, long Denominator)[] Selectivities =
    [
        ("equality", 1L, 200L),
        ("inequality", 1L, 3L),
        ("paired_range", 1L, 200L),
        ("null_test", 1L, 200L),
        ("match", 1L, 200L),
        ("matching", 1L, 100L),
        ("boolean", 1L, 2L),
        ("opaque", 1L, 3L),
    ];
    internal static readonly string[] SafeFunctions = ["lower", "upper", "length", "char_length", "character_length", "octet_length", "bit_length", "btrim", "ltrim", "rtrim", "reverse", "strpos", "position", "starts_with", "ascii", "initcap", "md5"];
    internal static readonly (SqlType, SqlType)[] SafeCasts =
    [
        (SqlType.I16, SqlType.I32),
        (SqlType.I16, SqlType.I64),
        (SqlType.I32, SqlType.I64),
        (SqlType.I16, SqlType.Decimal),
        (SqlType.I32, SqlType.Decimal),
        (SqlType.I64, SqlType.Decimal),
        (SqlType.I16, SqlType.F64),
        (SqlType.I32, SqlType.F64),
        (SqlType.I64, SqlType.F64),
        (SqlType.F32, SqlType.F64),
        (SqlType.I16, SqlType.F32),
        (SqlType.I32, SqlType.F32),
        (SqlType.I64, SqlType.F32),
        (SqlType.Boolean, SqlType.I32),
        (SqlType.I32, SqlType.Boolean),
        (SqlType.Uuid, SqlType.Text),
        (SqlType.I16, SqlType.Text),
        (SqlType.I32, SqlType.Text),
        (SqlType.I64, SqlType.Text),
        (SqlType.Decimal, SqlType.Text),
        (SqlType.F32, SqlType.Text),
        (SqlType.F64, SqlType.Text),
        (SqlType.Boolean, SqlType.Text),
    ];
    internal static readonly (SqlType, SqlType)[] NonEqualityCasts =
    [
        (SqlType.Decimal, SqlType.Text),
        (SqlType.F32, SqlType.Text),
        (SqlType.F64, SqlType.Text),
    ];
}
