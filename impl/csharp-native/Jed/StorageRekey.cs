using System.Security.Cryptography;

namespace Jed;

internal sealed partial class StorageTree
{
    // Reinsert in ascending encoded-key order. Settle every insertion into a
    // bounded scratch pager so the rebuild never retains the complete dirty tree.
    internal async ValueTask<StoragePageReference> RebuildImageAsync(StorageImageWriter output, ScratchHost scratch,
        Func<int, byte[]> entropy, long cacheBytes, SqlType[]? gistTypes, CancellationToken token)
    {
        await using var store = scratch.CreateSpillable(Math.Max(output.PageSize, cacheBytes));
        StoragePageCodec? codec = null;
        if (output.Encrypted)
        {
            var key = StorageKeyBlock.Entropy(entropy, 32);
            try { codec = new(key, entropy); } finally { CryptographicOperations.ZeroMemory(key); }
        }
        using var owned = codec;
        var targetPager = new StoragePager(store, output.PageSize, cacheBytes, codec);
        var meta = new StorageMeta(output.PageSize, 1, 0, output.Encrypted ? 4u : 2u, 0, 0);
        var recovered = new StorageRecovered(meta, 0, 0, [], [], [])
            { Encrypted = output.Encrypted ? new(meta, new byte[16], new byte[16]) : null };
        var committer = new StorageCommitter(targetPager, recovered);
        var tree = new StorageTree(targetPager, schema);
        await foreach (var record in ScanAsync(token).ConfigureAwait(false))
        {
            var materialized = await MaterializeRecordAsync(record, token).ConfigureAwait(false);
            tree = await tree.UpsertAsync(tree.PrepareRecord(materialized), token).ConfigureAwait(false);
            var allocation = new StorageAllocation(committer.Current, codec: codec);
            var live = checked((uint)(committer.Current.Meta.LivePages + tree.PageDelta));
            var root = await tree.SerializeBoundAsync(allocation, token).ConfigureAwait(false);
            var plan = allocation.PrepareMemory(root.Page, live);
            await committer.StageMemoryAsync(plan, token).ConfigureAwait(false); committer.AdoptStagedMemory(plan);
            tree = new(targetPager, schema, root.Page, plan.Meta.PageCount, root.Tag, tree.SerializedShape);
        }
        if (gistTypes is not null)
            return await (await GistTree.BuildAsync(tree, gistTypes, token).ConfigureAwait(false)).SerializeImageAsync(output, token).ConfigureAwait(false);
        return await tree.CopyImageAsync(output, token).ConfigureAwait(false);
    }
}
