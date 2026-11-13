namespace Jed;

public sealed partial class Database
{
    // Internal conformance seam. The simulator checks the live allocator as well
    // as a separately reopened image, so resident references cannot hide behind
    // a correct on-disk walk. No diagnostic mutates the storage state.
    internal StorageRecovered CommittedStorageForTesting()
    { lock (gate) { Check(); return committer.Current; } }
}
