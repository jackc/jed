// Generated data from 6b2355d98a7fd3738c60474779d1d63548f350dc.
// Regenerate only with mise run csharp-native:codegen.
namespace Jed;

internal static class RangeCatalog
{
    internal static readonly RangeFact[] Facts =
    [
        new("i32range", SqlType.I32, ["int4range"], true),
        new("i64range", SqlType.I64, ["int8range"], true),
        new("numrange", SqlType.Decimal, [], false),
        new("tsrange", SqlType.Timestamp, [], false),
        new("tstzrange", SqlType.Timestamptz, [], false),
        new("daterange", SqlType.Date, [], true),
    ];
}
