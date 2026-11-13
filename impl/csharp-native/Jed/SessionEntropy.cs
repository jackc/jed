using System.Security.Cryptography;

namespace Jed;

public sealed partial class Session
{
    internal UuidContext NewUuidContext(QueryOptions? query = null)
        => new(query?.RandomSource ?? options.RandomSource ?? Environment.RandomSource.Fill);
}
