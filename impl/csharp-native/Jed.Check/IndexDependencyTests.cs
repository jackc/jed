using System.Buffers.Binary;
using Jed;

internal static class IndexDependencyTests
{
    internal static async Task Run(string root)
    {
        var bytes = await File.ReadAllBytesAsync(Path.Combine(root, "spec/tz/fixtures/tzdata.jtz"));
        var original = TimeZoneData.Load(bytes);
        var changedBytes = bytes.ToArray(); changedBytes[10] = changedBytes[10] == (byte)'x' ? (byte)'y' : (byte)'x';
        BinaryPrimitives.WriteUInt32BigEndian(changedBytes.AsSpan(changedBytes.Length - 4), StorageFormat.Crc32(changedBytes.AsSpan(0, changedBytes.Length - 4)));
        var changed = TimeZoneData.Load(changedBytes);
        await using var database = await Database.CreateInMemoryAsync();
        await using var session = database.CreateSession(new SessionOptions { TimeZones = original });
        await Exec(session, "CREATE TABLE events (id i32 PRIMARY KEY, local timestamp, zone text)");
        await Exec(session, "INSERT INTO events VALUES (1,'2024-01-15 12:00:00','America/New_York')");
        await Exec(session, "CREATE INDEX named ON events ((local AT TIME ZONE 'America/New_York'))");
        await Exec(session, "CREATE INDEX dynamic_zone ON events ((local AT TIME ZONE zone))");
        using (var pin = database.Pin())
        {
            var indexes = pin.Snapshot.Catalog.Tables.Single().Indexes!;
            var named = indexes.Single(index => index.Name == "named");
            Corpus.Equal(original.Version, named.TimezoneDependencies![0].Version, "static zone version is persisted");
            var dynamic = indexes.Single(index => index.Name == "dynamic_zone");
            Corpus.Equal(true, dynamic.DynamicTimezone, "dynamic zone flag");
            Corpus.Equal(original.ZoneNames.Count + original.Links.Count, dynamic.TimezoneDependencies!.Length, "dynamic dependencies include aliases");
        }
        await using var missing = database.CreateSession();
        await Exec(missing, "SELECT id FROM events");
        await HostTests.Error(SqlStates.CollationVersionMismatch, () => Exec(missing, "UPDATE events SET id = 2"));
        await HostTests.Error(SqlStates.InvalidParameterValue, () => Exec(missing, "REINDEX INDEX named"));
        await using var updated = database.CreateSession(new SessionOptions { TimeZones = changed });
        await HostTests.Error(SqlStates.CollationVersionMismatch, () => Exec(updated, "UPDATE events SET id = 2"));
        await Exec(updated, "BEGIN"); await Exec(updated, "REINDEX TABLE events"); await Exec(updated, "ROLLBACK");
        await HostTests.Error(SqlStates.CollationVersionMismatch, () => Exec(updated, "UPDATE events SET id = 2"));
        await Exec(updated, "REINDEX TABLE events"); await Exec(updated, "UPDATE events SET id = 2");
        await HostTests.Error(SqlStates.CollationVersionMismatch, () => Exec(session, "UPDATE events SET id = 3"));
        await using var restored = await Database.DeserializeAsync(await database.SerializeAsync());
        await using var restoredSession = restored.CreateSession(new SessionOptions { TimeZones = changed });
        await Exec(restoredSession, "UPDATE events SET id = 3");
        await HostTests.Error(SqlStates.InvalidObjectDefinition, () => Exec(restoredSession, "CREATE INDEX local_literal ON events ((TIMESTAMPTZ '2024-01-01 00:00:00'))"));
        await Exec(restoredSession, "CREATE INDEX utc_literal ON events ((TIMESTAMPTZ '2024-01-01 00:00:00+00'))");
        Console.WriteLine("PASS persisted index dependencies: named/dynamic alias pins, missing/skewed environments, atomic REINDEX rollback, byte round-trip, literal zone provenance");
    }
    private static async Task Exec(Session session, string sql)
    {
        await using var rows = await session.QueryAsync(sql);
        await foreach (var unused in rows) { }
    }
}
