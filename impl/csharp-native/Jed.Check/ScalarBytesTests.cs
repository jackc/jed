using Jed;

// Ownership and preallocation-reservation invariants that SQL corpus cannot see.
internal static class ScalarBytesTests
{
    internal static void Run()
    {
        byte[] input = [1, 2, 255];
        var changed = Bytea.SetByte(input, 1, 3); changed[0] = 0;
        Corpus.Equal((byte)1, input[0], "set_byte result owns storage");
        Corpus.Equal((byte)2, input[1], "set_byte keeps source immutable");
        var sliced = Bytea.Substring(input, 1); sliced[0] = 0;
        Corpus.Equal((byte)1, input[0], "substring result owns storage");
        var trimmed = Bytea.Trim(input, []); trimmed[0] = 0;
        Corpus.Equal((byte)1, input[0], "trim result owns storage");
        var id = Uuid.Parse("00112233-4455-6677-8899-aabbccddeeff");
        Corpus.Equal("00112233445566778899aabbccddeeff", Convert.ToHexStringLower(id), "UUID bytes are network order, not Guid native order");
        Corpus.Equal("00112233-4455-6677-8899-aabbccddeeff", Uuid.Render(id), "UUID network-order roundtrip");
        foreach (var format in new[] { "hex", "base64", "escape" })
        {
            long reserved = -1;
            var encoded = Bytea.Encode(input, format, length => reserved = length);
            Corpus.Equal((long)encoded.Length, reserved, "encode reserves exact ASCII output length");
            var decoded = Bytea.Decode(encoded, format, length => reserved = length);
            Corpus.Equal((long)decoded.Length, reserved, "decode reserves exact output length");
            Corpus.Equal(Convert.ToHexString(input), Convert.ToHexString(decoded), "codec roundtrip");
            try { _ = Bytea.Encode(input, format, _ => throw new ReservationDenied()); }
            catch (ReservationDenied) { continue; }
            throw new InvalidDataException("encoder ignored reservation failure");
        }
        Console.WriteLine("PASS binary kernel invariants: owned output, network-order UUID, exact preallocation reservations");
    }
    private sealed class ReservationDenied : Exception;
}
