using System.Globalization;
using Jed;

internal static partial class Corpus
{
    private static async Task RunDisk(string file, List<CorpusRecord> records, SessionOptions sessionOptions,
        Action<string, string, string>? transcript)
    {
        using var backing = new CorpusFile();
        using var credential = transcript is null ? null : DatabaseCredential.FromKey(Enumerable.Repeat((byte)0x4a, 32).ToArray());
        var options = new DatabaseFileOptions { Locking = FileLocking.None,
            Credential = credential, Environment = DatabaseEnvironment.Builder().WithLoadedData(sessionOptions.UnicodeData, sessionOptions.TimeZones).Build() };
        await using (var initial = await Database.CreateFileAsync(backing.Path, options).ConfigureAwait(false)) { }
        long storageLimit = 0;
        foreach (var record in records)
        {
            try
            {
                // The complete handle and session are retired at every boundary.
                // Only committed bytes, database limit and record directives carry.
                await using var database = await Database.OpenFileAsync(backing.Path, options).ConfigureAwait(false);
                database.MaxStorageBytes = storageLimit;
                await using var session = database.CreateSession(sessionOptions);
                await RunRecord(session, record, transcript).ConfigureAwait(false);
                storageLimit = database.MaxStorageBytes;
            }
            catch (Exception error) { throw new InvalidDataException($"{file}:{record.Line}: {record.Sql}\n{error}", error); }
        }
    }
}
