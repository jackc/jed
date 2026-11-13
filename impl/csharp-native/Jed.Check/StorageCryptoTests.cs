using System.Buffers.Binary;
using System.Security.Cryptography;
using Jed;

internal static class StorageCryptoTests
{
    internal static async Task Recovery(string root)
    {
        var host = CryptoTests.ReadVectors(Path.Combine(root, "spec/fileformat/encryption_vectors.toml")).Single(v => v.Name == "host_kek");
        using var credential = StorageCredential.HostKey(CryptoTests.Hex(host, "key_hex"));
        using var wrong = StorageCredential.HostKey(new byte[32]);
        var files = Directory.GetFiles(Path.Combine(root, "spec/fileformat/fixtures"), "encrypted_*.jed").Order(StringComparer.Ordinal).ToArray();
        foreach (var path in files)
        {
            var image = await File.ReadAllBytesAsync(path);
            await using var store = new MemoryBlockStore(image);
            using var recovered = await StorageCryptoRecovery.OpenAsync(store, credential, _ => throw new InvalidDataException("recovery must not draw entropy"));
            Corpus.Equal((uint)(image.Length / recovered.Meta.Header.PageSize), recovered.Meta.Header.PageCount, "encrypted recovered page count");
            var rootPage = new byte[recovered.Meta.Header.PageSize];
            await store.ReadAsync((long)recovered.Meta.Header.Root * rootPage.Length, rootPage);
            _ = recovered.Codec.OpenBody(recovered.Meta.Header.Root, rootPage, recovered.Meta.RootTag);
            var pager = new StoragePager(store, rootPage.Length, cacheBytes: rootPage.Length * 2, codec: recovered.Codec);
            var catalog = await Catalog.ReadAsync(pager, recovered.Meta);
            foreach (var table in catalog.Tables)
            {
                var tree = new StorageTree(pager, table.LeafSchema(), table.Root, recovered.Meta.Header.PageCount, table.RootTag);
                long rows = 0; await foreach (var row in tree.ScanAsync()) { _ = row.Key; rows++; }
                Corpus.Equal(table.RowCount, rows, "authenticated encrypted tree row count");
            }
            // Cache residency never relaxes bound-pointer authentication.
            _ = await pager.ReadPageAsync(new StoragePageReference(recovered.Meta.Header.Root, recovered.Meta.RootTag));
            var badTag = recovered.Meta.RootTag.ToArray(); badTag[0] ^= 1;
            try { _ = await pager.ReadPageAsync(new StoragePageReference(recovered.Meta.Header.Root, badTag)); throw new InvalidDataException("cached page ignored pointer tag"); }
            catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { }
            await Expect(SqlStates.InvalidPassword, store, wrong);
            await Expect(SqlStates.CredentialRequired, store, null);
            var size = recovered.Meta.Header.PageSize;
            // A CRC-valid selected meta whose tag fails must NOT use its valid sibling.
            var changed = image.AsSpan(0, size).ToArray(); changed[^1] ^= 1; StorageFormat.SealMeta(changed);
            await store.WriteAsync(0, changed); await Expect(SqlStates.DataCorrupted, store, credential);
            // A torn meta is different: its invalid CRC permits the other candidate.
            changed[32] ^= 1; await store.WriteAsync(0, changed);
            using var fallback = await StorageCryptoRecovery.OpenAsync(store, credential, count => new byte[count]);
            Corpus.Equal(1, fallback.Recovered.Slot, "encrypted torn meta falls back before authentication");
        }
        await MutateEncrypted(files[0], credential);
        Console.WriteLine($"PASS encrypted recovery: {files.Length} complete shared catalogs/trees, cached bound tags, encrypted split-tree COW SQL/recovery, credential errors, torn-meta fallback and no authentication fallback");
        static async Task Expect(string state, IBlockStore store, StorageCredential? credential)
        {
            try { using var unexpected = await StorageCryptoRecovery.OpenAsync(store, credential, count => new byte[count]); }
            catch (SqlException error) { Corpus.Equal(state, error.SqlState, "encrypted recovery SQLSTATE"); return; }
            throw new InvalidDataException("expected encrypted recovery error " + state);
        }
    }
    private static async Task MutateEncrypted(string path, StorageCredential credential)
    {
        var image = await File.ReadAllBytesAsync(path); var store = new MemoryBlockStore(image);
        await using var database = await Database.OpenEncryptedStoreAsync(store, credential, RandomNumberGenerator.GetBytes);
        await using var session = database.CreateSession();
        await DatabaseFileTests.Exec(session, "CREATE TABLE crypto_check (id i64 PRIMARY KEY, n i64)");
        await DatabaseFileTests.Exec(session, "BEGIN");
        for (var i = 0; i < 160; i++) await DatabaseFileTests.Exec(session, $"INSERT INTO crypto_check VALUES ({i},{i * 7})");
        await DatabaseFileTests.Exec(session, "COMMIT");
        Corpus.Equal("1113", await DatabaseFileTests.Scalar(session, "SELECT n FROM crypto_check WHERE id=159"), "encrypted COW SQL read");
        using var recovered = await StorageCryptoRecovery.OpenAsync(store, credential, RandomNumberGenerator.GetBytes);
        var pager = new StoragePager(store, recovered.Meta.Header.PageSize, codec: recovered.Codec);
        var catalog = await Catalog.ReadAsync(pager, recovered.Meta);
        var table = catalog.Tables.Single(table => table.Name == "crypto_check");
        var tree = new StorageTree(pager, table.LeafSchema(), table.Root, recovered.Meta.Header.PageCount, table.RootTag);
        var rows = new List<StorageRecord>(); await foreach (var row in tree.ScanAsync()) rows.Add(row);
        Corpus.Equal(160, rows.Count, "encrypted split tree survives independent recovery");
        Corpus.Equal("1113", table.DecodeRow(rows[^1])[1].ToString(), "encrypted stored value survives recovery");
    }
    internal static void Run(string root)
    {
        var vectors = CryptoTests.ReadVectors(Path.Combine(root, "spec/fileformat/encryption_vectors.toml"));
        var block = vectors.Single(v => v.Name == "key_block");
        var rewrap = vectors.Single(v => v.Name == "rewrap");
        var host = vectors.Single(v => v.Name == "host_kek");
        var dek = CryptoTests.Hex(block, "dek_hex");
        using var credential = StorageCredential.HostKey(CryptoTests.Hex(host, "key_hex"));
        using var replacement = StorageCredential.HostKey(CryptoTests.Hex(rewrap, "key_hex"));
        var first = CheckBlock(block, dek, credential, 1);
        var second = CheckBlock(rewrap, dek, replacement, 2);
        var selected = StorageKeyBlock.Select(first.Bytes.Span, first.Bytes.Span);
        Corpus.Equal(false, selected.NeedsRepair, "equal key blocks do not need repair");
        Corpus.Equal(2u, selected.Page, "equal key block selects page 2");
        selected = StorageKeyBlock.Select(first.Bytes.Span, second.Bytes.Span);
        Corpus.Equal(3u, selected.Page, "newest key block selected");
        Corpus.Equal(true, selected.NeedsRepair, "unequal blocks need repair");
        Error(SqlStates.InvalidPassword, () => selected.Block.Unwrap(credential));
        Error(SqlStates.CredentialRequired, () => selected.Block.Unwrap(null));
        var broken = first.Bytes.ToArray(); broken[0] ^= 1;
        Corpus.Equal(3u, StorageKeyBlock.Select(broken, first.Bytes.Span).Page, "invalid first copy repaired from second");
        Error(SqlStates.DataCorrupted, () => StorageKeyBlock.Select(broken, broken));
        var conflict = first.Bytes.ToArray(); conflict[32] ^= 1; ResealKeyCrc(conflict);
        Error(SqlStates.DataCorrupted, () => StorageKeyBlock.Select(first.Bytes.Span, conflict));
        var hostile = first.Bytes.ToArray(); hostile[7] = 1;
        BinaryPrimitives.WriteUInt32BigEndian(hostile.AsSpan(16), uint.MaxValue);
        BinaryPrimitives.WriteUInt32BigEndian(hostile.AsSpan(20), 1);
        BinaryPrimitives.WriteUInt32BigEndian(hostile.AsSpan(24), 1); ResealKeyCrc(hostile);
        Corpus.Equal<StorageKeyBlock?>(null, StorageKeyBlock.TryParse(hostile), "KDF bounds reject before derivation");

        var files = Directory.GetFiles(Path.Combine(root, "spec/fileformat/fixtures"), "encrypted_*.jed").Order(StringComparer.Ordinal).ToArray();
        Corpus.Equal(6, files.Length, "frozen encrypted golden inventory");
        var opened = 0;
        foreach (var path in files)
        {
            var image = File.ReadAllBytes(path);
            var size = checked((int)BinaryPrimitives.ReadUInt32BigEndian(image.AsSpan(8)));
            var selection = StorageKeyBlock.Select(image.AsSpan(2 * size, size), image.AsSpan(3 * size, size));
            var dataKey = selection.Block.Unwrap(credential);
            byte[] nonce = [];
            byte[] Next(int length) { Corpus.Equal(length, nonce.Length, "golden nonce length"); return nonce.ToArray(); }
            try
            {
                using var codec = new StoragePageCodec(dataKey, Next);
                for (uint slot = 0; slot < 2; slot++)
                {
                    var meta = image.AsSpan((int)slot * size, size).ToArray();
                    codec.AuthenticateMeta(slot, meta, []);
                    nonce = meta.AsSpan(size - 40, 24).ToArray();
                    Bytes(meta, codec.SealMeta(slot, meta, []), "golden meta reseal");
                    Error(SqlStates.DataCorrupted, () => codec.AuthenticateMeta(1 - slot, meta, []));
                    meta[20] ^= 1;
                    Error(SqlStates.DataCorrupted, () => codec.AuthenticateMeta(slot, meta, []));
                }
                for (uint page = 4; page < image.Length / size; page++)
                {
                    var stored = image.AsSpan((int)page * size, size).ToArray();
                    var tag = stored.AsSpan(size - 16).ToArray();
                    var plain = codec.OpenBody(page, stored, tag);
                    nonce = stored.AsSpan(size - 40, 24).ToArray();
                    Bytes(stored, codec.SealBody(page, plain), "golden body reseal");
                    Error(SqlStates.DataCorrupted, () => codec.OpenBody(page + 1, stored, tag));
                    tag[0] ^= 1;
                    Error(SqlStates.DataCorrupted, () => codec.OpenBody(page, stored, tag));
                    CryptographicOperations.ZeroMemory(plain); opened++;
                }
            }
            finally { CryptographicOperations.ZeroMemory(dataKey); }
        }
        // Manifest-overflow pages are plaintext but authenticated as an ordered
        // sequence of (id, stored bytes). No golden currently contains one.
        var metaInput = new byte[1024]; "JEDB"u8.CopyTo(metaInput); metaInput[6] = 1; metaInput[7] = 40;
        using (var codec = new StoragePageCodec(dek, count => new byte[count]))
        {
            StorageManifestPage[] manifest = [new(4, new byte[1024]), new(5, Enumerable.Repeat((byte)1, 1024).ToArray())];
            var sealedMeta = codec.SealMeta(0, metaInput, manifest);
            codec.AuthenticateMeta(0, sealedMeta, manifest);
            Error(SqlStates.DataCorrupted, () => codec.AuthenticateMeta(0, sealedMeta, [manifest[1], manifest[0]]));
            Error(SqlStates.DataCorrupted, () => codec.AuthenticateMeta(0, sealedMeta, [new(6, manifest[0].Bytes), manifest[1]]));
        }
        CryptographicOperations.ZeroMemory(dek);
        Console.WriteLine($"PASS encrypted codec: {files.Length} shared golden files, {opened} body pages, both meta slots, key-block create/rewrap/selection and authentication failures");
    }
    private static StorageKeyBlock CheckBlock(CryptoTests.Vector vector, byte[] dek, StorageCredential credential, ulong generation)
    {
        var expected = new byte[1024]; CryptoTests.Hex(vector, "block_hex").CopyTo(expected, 0);
        var pending = new Queue<byte[]>([expected.AsSpan(32, 32).ToArray(), expected.AsSpan(64, 12).ToArray()]);
        byte[] Next(int length) { var draw = pending.Dequeue(); Corpus.Equal(length, draw.Length, "key-block salt/nonce draw order"); return draw; }
        var created = StorageKeyBlock.Create(1024, generation, dek, credential, Next);
        Corpus.Equal(0, pending.Count, "key-block exact draw count");
        Bytes(expected, created.Bytes.ToArray(), "shared key block create");
        Bytes(dek, created.Unwrap(credential), "shared key block unwrap");
        return created;
    }
    private static void ResealKeyCrc(byte[] bytes) => BinaryPrimitives.WriteUInt32BigEndian(bytes.AsSpan(124), StorageFormat.Crc32(bytes.AsSpan(0, 124)));
    private static void Bytes(byte[] expected, byte[] actual, string label) => Corpus.Equal(Convert.ToHexString(expected), Convert.ToHexString(actual), label);
    private static void Error(string state, Action action)
    {
        try { action(); }
        catch (SqlException error) { Corpus.Equal(state, error.SqlState, "encrypted codec SQLSTATE"); return; }
        throw new InvalidDataException("expected encrypted codec error " + state);
    }
}
