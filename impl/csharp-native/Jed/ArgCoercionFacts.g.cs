// Generated data from 6b2355d98a7fd3738c60474779d1d63548f350dc.
// Regenerate only with mise run csharp-native:codegen.
namespace Jed;

internal static class ArgCoercionFacts
{
    internal static readonly (string From, SqlType To, int Cost)[] Rows =
    [
        ("i16", SqlType.I32, 1),
        ("i32", SqlType.I64, 1),
        ("i16", SqlType.I64, 2),
        ("f32", SqlType.F64, 1),
        ("integer_literal", SqlType.I32, 1),
        ("integer_literal", SqlType.I16, 2),
        ("i16", SqlType.Decimal, 3),
        ("i32", SqlType.Decimal, 3),
        ("i64", SqlType.Decimal, 3),
        ("integer_literal", SqlType.F64, 4),
        ("integer_literal", SqlType.F32, 5),
        ("decimal_literal", SqlType.F64, 4),
        ("decimal_literal", SqlType.F32, 5),
    ];
}
