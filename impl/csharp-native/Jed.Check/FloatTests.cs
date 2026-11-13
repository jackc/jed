using Jed;

internal static class FloatTests
{
    internal static void Run()
    {
        // Bit preservation and the exact-dyadic bridge cannot be asserted through
        // R's intentionally tolerant SQL rendering. Fixed patterns include signed
        // zeros and subnormals; the deterministic walk is independent of host RNG.
        ulong state = 0x4a454442;
        foreach (var single in new[] { false, true })
        {
            for (var i = 0; i < 128; i++)
            {
                state ^= state << 13; state ^= state >> 7; state ^= state << 17;
                var bits = i switch { 0 => 0ul, 1 => single ? 0x80000000ul : 0x8000000000000000ul, 2 => 1ul, _ => state };
                var value = single ? BitConverter.UInt32BitsToSingle(unchecked((uint)bits)) : BitConverter.UInt64BitsToDouble(bits);
                if (!double.IsFinite(value)) continue;
                var encoded = FloatValue.Render(value, single);
                var decoded = FloatValue.Parse(encoded, single);
                Corpus.Equal(BitConverter.DoubleToUInt64Bits(value), BitConverter.DoubleToUInt64Bits(decoded), "strict-shortest text preserves IEEE bits");
                if (value == 0) continue;
                var exact = FloatValue.ToDecimal(value);
                var fromExact = FloatValue.Parse(exact.ToString(), single);
                Corpus.Equal(BitConverter.DoubleToUInt64Bits(value), BitConverter.DoubleToUInt64Bits(fromExact), "exact dyadic decimal preserves IEEE bits");
            }
        }
        Console.WriteLine("PASS float bit invariants: signed-zero/subnormal and deterministic IEEE pattern roundtrips through PG text and exact decimal");
    }
}
