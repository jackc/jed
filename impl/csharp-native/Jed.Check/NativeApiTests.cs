using Jed;

internal static class NativeApiTests
{
    private static void Check(bool condition, string reason) { if (!condition) throw new InvalidDataException(reason); }
    private static void Error(string code, Action action)
    {
        try { action(); throw new InvalidDataException("expected " + code); }
        catch (SqlException error) when (error.SqlState == code) { }
    }
    internal static async Task Run()
    {
        await using var session = new Session(new SessionOptions { TimeZone = "-09:00" });
        await session.ExecuteAsync("CREATE TABLE native_api (id i32 PRIMARY KEY, label text)");
        Check(await session.ExecuteAsync("INSERT INTO native_api VALUES ($1, $2)", [42, "owned"]) == 1, "native command count");
        var row = await session.QueryFirstOrDefaultAsync("SELECT id, label, NULL::i32 FROM native_api WHERE id = $1", [42]);
        Check(row is not null && row.Get<int>(0) == 42 && row.Get<string>(1) == "owned" && row.Get<int?>(2) is null, "native getters");
        Check(row!.Values()[0] is long && row.Values()[2] is null, "dynamic values preserve native null and integer");
        Error("22004", () => row.Get<int>(2));
        Error("22003", () => SqlValue.Integer(32768).Get<short>());
        Error("22003", () => SqlValue.From(ulong.MaxValue));
        Error("22003", () => SqlValue.Decimal(ExactDecimal.Parse("0.12345678901234567890123456789")).Get<decimal>());
        Check(SqlValue.Decimal(ExactDecimal.Parse("123.45")).Get<decimal>() == 123.45m, "exact CLR decimal conversion");
        var bytes = new byte[] { 1, 2, 3 }; SqlValue binary = bytes; bytes[0] = 9;
        Check(binary.Get<byte[]>()[0] == 1, "native byte parameter owns a copy");
        var uuid = Guid.Parse("00112233-4455-6677-8899-aabbccddeeff"); SqlValue uuidValue = uuid;
        Check(uuidValue.Get<Guid>() == uuid, "native uuid byte order");
        var time = new DateTimeOffset(2000, 1, 1, 1, 2, 3, TimeSpan.FromHours(14));
        var temporal = await session.QueryFirstOrDefaultAsync("SELECT $1::date, $2::timestamp, $3::timestamptz, ($4::timestamp[])[1]", [time, time, time, SqlValue.From(new[] { time })]);
        Check(temporal!.Get<DateOnly>(0) == new DateOnly(2000, 1, 1), "host time date uses own calendar");
        Check(temporal.Get<DateTime>(1) == time.DateTime && temporal.Get<DateTime>(1).Kind == DateTimeKind.Unspecified, "host timestamp uses own fields");
        Check(temporal.Get<DateTimeOffset>(2) == time && temporal.Get<DateTime>(3) == time.DateTime, "host instant and array calendar coercion");
        Check(temporal.Values()[1] is DateTime && temporal.Values()[2] is DateTimeOffset, "dynamic temporal distinction");
        Error("22008", () => SqlValue.Timestamp(long.MaxValue).Get<DateTime>());
        Check(SqlValue.Timestamp(long.MaxValue).ToNative() is SqlValue, "infinity retains raw SQL representation");
        Error("42804", () => SqlValue.From(DateTime.SpecifyKind(DateTime.Now, DateTimeKind.Local)));
        try
        {
            await session.QueryFirstOrDefaultAsync("SELECT $1::bool", [42]);
            throw new InvalidDataException("native parameter bypassed SQL type checking");
        }
        catch (SqlException error) when (error.SqlState == "42804") { }
        Check(await session.QueryFirstOrDefaultAsync("SELECT id FROM native_api WHERE false") is null, "empty first-row helper");
        Check((await session.QueryFirstOrDefaultAsync("SELECT $1::i32", options: new() { Parameters = [17] }))!.Get<int>(0) == 17, "helper preserves options parameter binding");
        await using (var rows = await session.QueryAsync("SELECT id AS \"MiXeD\" FROM native_api"))
            Check(rows.GetOrdinal("MiXeD") == 0 && rows.GetOrdinal("mixed") == 0, "ordinal name lookup");
        await foreach (var mapped in session.QueryAsync("SELECT id FROM native_api", value => value.Get<int>(0)))
        { Check(mapped == 42, "mapped async iterator"); break; }
        Check((await session.QueryFirstOrDefaultAsync("SELECT 1"))!.Get<int>(0) == 1, "iterator break releases session");
        try
        {
            await foreach (var _ in session.QueryAsync<int>("SELECT id FROM native_api", _ => throw new InvalidDataException("mapper"))) { }
            throw new InvalidDataException("mapper did not run");
        }
        catch (InvalidDataException error) when (error.Message == "mapper") { }
        Check((await session.QueryFirstOrDefaultAsync("SELECT 2"))!.Get<int>(0) == 2, "mapper failure releases session");
        Console.WriteLine("PASS native API: checked parameters/results, owned values, temporal calendar/instant binding and mapped cursor cleanup");
    }
}
