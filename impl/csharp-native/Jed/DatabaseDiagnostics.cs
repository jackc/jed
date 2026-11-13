namespace Jed;

public sealed partial class Database
{
    // Internal conformance seam. The simulator checks the live allocator as well
    // as a separately reopened image, so resident references cannot hide behind
    // a correct on-disk walk. No diagnostic mutates the storage state.
    internal StorageRecovered CommittedStorageForTesting()
    { lock (gate) { Check(); return committer.Current; } }

    // An acknowledged relaxed commit remains published if its subsequent cap
    // flush poisons storage. The simulator must observe that publication without
    // making a public read (which correctly fails), then validate crash recovery.
    internal (ulong Version, StorageRecovered Storage, string[] Attachments, ulong Oldest) PublishedStateForTesting()
    {
        lock (gate)
            return (current.Meta.TransactionId, committer.Current, attachments.Keys.Order(Utf8NameComparer.Instance).ToArray(),
                pins.Count == 0 ? current.Meta.TransactionId : pins.Keys.Min());
    }
}
