namespace Jed;

// Dictionary identity for already encoded keys. Observable replay order is
// carried separately by ordinals; the runtime hash never reaches storage.
internal sealed class ByteKeyComparer : IEqualityComparer<byte[]>
{
    internal static readonly ByteKeyComparer Instance = new();
    public bool Equals(byte[]? a, byte[]? b) => ReferenceEquals(a, b) ||
        a is not null && b is not null && a.AsSpan().SequenceEqual(b);
    public int GetHashCode(byte[] bytes)
    {
        var hash = new HashCode(); hash.AddBytes(bytes); return hash.ToHashCode();
    }
}
