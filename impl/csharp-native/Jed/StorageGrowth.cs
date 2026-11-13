namespace Jed;

internal static class StorageGrowth
{
    // pager.md §7: small files double, with a 16 KiB floor and 1 MiB cap
    // on each increment. The host durably allocates the resulting zero tail.
    internal static long Length(long currentBytes, uint requiredPages, int pageSize)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(currentBytes);
        ArgumentOutOfRangeException.ThrowIfNegativeOrZero(pageSize);
        var pages = currentBytes / pageSize;
        var floor = Math.Max(1, 16384 / pageSize);
        var cap = Math.Max(1, 1048576 / pageSize);
        while (pages < requiredPages) pages = checked(pages + Math.Clamp(pages, floor, cap));
        return checked(pages * pageSize);
    }
}
