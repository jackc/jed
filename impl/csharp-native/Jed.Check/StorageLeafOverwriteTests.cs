using Jed;

internal static class StorageLeafOverwriteTests
{
    internal static void Run()
    {
        var schema = new StorageLeafSchema([new(8, 0, Integer: true), new(8, Integer: true), new(1), new(Dictionary: true)], 8);
        var records = Enumerable.Range(0, 400).Select(i => new StorageRecord(KeyEncoding.Integer(i, 8),
            [KeyEncoding.Integer(i, 8), KeyEncoding.Integer(i % 13, 8), [(byte)(i & 1)], [5, 97, 98, 99]])).ToArray();
        foreach (var encrypted in new[] { false, true })
        {
            var initial = StorageLeaf.Encode(8192, schema, records, encrypted);
            var decoded = (byte[])initial.Clone();
            if (encrypted) decoded.AsSpan(12, 4).Clear();
            var packed = StorageLeaf.Parse(decoded, schema, encrypted);
            foreach (var at in new[] { 0, 7, 8, 199, 399 })
                foreach (var value in new byte[]?[] { null, KeyEncoding.Integer(0, 8), KeyEncoding.Integer(long.MaxValue, 8), KeyEncoding.Integer(long.MinValue, 8) })
                {
                    var cells = (byte[]?[])records[at].Cells.Clone(); cells[1] = value; cells[2] = null;
                    var replacement = records[at] with { Cells = cells };
                    var changed = packed.ReplaceFixed(at, replacement, encrypted) ?? throw new InvalidDataException("fixed overwrite unexpectedly fell back");
                    var all = (StorageRecord[])records.Clone(); all[at] = replacement;
                    var expected = StorageLeaf.Encode(8192, schema, all, encrypted);
                    Corpus.Equal(true, expected.AsSpan().SequenceEqual(changed.EncodedPage.Span), "packed overwrite equals complete canonical re-encode");
                    Corpus.Equal(true, decoded.AsSpan().SequenceEqual(packed.EncodedPage.Span), "packed overwrite preserves prior snapshot bytes");
                    var reverted = changed.ReplaceFixed(at, records[at], encrypted)!;
                    Corpus.Equal(true, initial.AsSpan().SequenceEqual(reverted.EncodedPage.Span), "packed overwrite reselects compact encoding after range shrinks");
                }
            var variable = records[0] with { Cells = [records[0].Cells[0], records[0].Cells[1], records[0].Cells[2], [5, 100]] };
            Corpus.Equal(true, packed.ReplaceFixed(0, variable, encrypted) is null, "variable edit takes general immutable rewrite path");
        }
        Console.WriteLine("PASS packed fixed-cell overwrites: full canonical byte comparison, NULL bitmap edges, integer range widening/shrinking and immutable source bytes");
    }
}
