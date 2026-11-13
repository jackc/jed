using System.Buffers.Binary;

namespace Jed;

/// <summary>Reproducible splitmix64 source. This is not a cryptographically secure random generator.</summary>
public sealed class SeededRandomSource(ulong seed) : IRandomSource
{
    private readonly object gate = new();
    private ulong state = seed;
    public void Reseed(ulong seed) { lock (gate) state = seed; }
    public byte[] Fill(int count)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(count);
        var result = new byte[count];
        lock (gate)
        {
            for (var at = 0; at < count;)
            {
                ulong value;
                unchecked
                {
                    state += 0x9E3779B97F4A7C15;
                    value = state; value = (value ^ (value >> 30)) * 0xBF58476D1CE4E5B9;
                    value = (value ^ (value >> 27)) * 0x94D049BB133111EB; value ^= value >> 31;
                }
                var take = Math.Min(8, count - at);
                for (var i = 0; i < take; i++) result[at + i] = unchecked((byte)(value >> (56 - i * 8)));
                at += take;
            }
        }
        return result;
    }
}

internal sealed class UuidContext(Func<int, byte[]> source)
{
    private ulong counter;
    internal byte[] Draw(int count)
    {
        try
        {
            var bytes = source(count);
            if (bytes is null || bytes.Length != count) throw new SqlException(SqlStates.IoError, "random source returned the wrong byte count");
            return bytes.ToArray(); // A host may reuse its own buffer.
        }
        catch (IOException error) { throw new SqlException(SqlStates.IoError, error.Message); }
    }
    internal int Counter() { var value = (int)(counter & 4095); counter = unchecked(counter + 1); return value; }
}

internal static class UuidFunctions
{
    internal static SqlValue Call(string name, SqlValue[] args, Meter meter)
    {
        if (name is "uuidv4" or "gen_random_uuid")
        {
            var bytes = meter.Uuid.Draw(16);
            bytes[6] = (byte)((bytes[6] & 15) | 64); bytes[8] = (byte)((bytes[8] & 63) | 128);
            return SqlValue.OwnedBytes(bytes, SqlType.Uuid);
        }
        if (name == "uuidv7")
        {
            var micros = meter.Temporal.StatementMicros;
            if (args.Length != 0) micros = TemporalExpression.Binary("+", SqlValue.Timestamp(micros, true), args[0], SqlType.Timestamptz, meter).AsTimestamp();
            if (micros < 0 || micros / 1000 >= 1L << 48)
                throw new SqlException(SqlStates.DatetimeFieldOverflow, "UUID timestamp outside its 48-bit millisecond range");
            var millis = micros / 1000; var counter = meter.Uuid.Counter(); var bytes = new byte[16];
            for (var i = 0; i < 6; i++) bytes[i] = unchecked((byte)(millis >> (40 - i * 8)));
            bytes[6] = (byte)(112 | (counter >> 8)); bytes[7] = unchecked((byte)counter);
            meter.Uuid.Draw(8).CopyTo(bytes, 8); bytes[8] = (byte)((bytes[8] & 63) | 128);
            return SqlValue.OwnedBytes(bytes, SqlType.Uuid);
        }
        var input = args[0].ByteSpan; var result = name == "uuid_extract_version" ? SqlType.I16 : SqlType.Timestamptz;
        if ((input[8] & 192) != 128) return SqlValue.Null(result);
        var version = input[6] >> 4;
        if (name == "uuid_extract_version") return SqlValue.Integer(version, SqlType.I16);
        if (version == 7)
        {
            long millis = 0; for (var i = 0; i < 6; i++) millis = (millis << 8) | input[i];
            return SqlValue.Timestamp(millis * 1000, true);
        }
        if (version != 1) return SqlValue.Null(result);
        var ticks = (long)(BinaryPrimitives.ReadUInt16BigEndian(input[6..]) & 4095) << 48 |
            (long)BinaryPrimitives.ReadUInt16BigEndian(input[4..]) << 32 | BinaryPrimitives.ReadUInt32BigEndian(input);
        return SqlValue.Timestamp((ticks - 122192928000000000L) / 10, true);
    }
}
