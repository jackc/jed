namespace Jed;

/// <summary>Create a fresh database. An absent path selects a private in-memory database.</summary>
public sealed record DatabaseCreateOptions : DatabaseFileOptions
{
    public string? Path { get; init; }
    public int PageSize { get; init; } = 8192;
}

/// <summary>Outside inputs and committed-storage allowance for an imported in-memory image.</summary>
public sealed record DatabaseDeserializeOptions
{
    public DatabaseEnvironment? Environment { get; init; }
    public long MaxStorageBytes { get; init; }
}

public sealed partial class Database
{
    public static async Task<Database> CreateAsync(DatabaseCreateOptions? options = null, CancellationToken cancellationToken = default)
    {
        options ??= new();
        if (options.MaxStorageBytes < 0) throw new ArgumentOutOfRangeException(nameof(options));
        if (!StorageFormat.ValidPageSize(options.PageSize))
            throw new SqlException(SqlStates.FeatureNotSupported, options.PageSize < 256 ? "page size too small" :
                options.PageSize > 65536 ? "page size too large" : "page size must be a power of two");
        if (options.Path is { } path)
            return await CreateFileAsync(path, options, options.PageSize, cancellationToken).ConfigureAwait(false);
        if (options.Credential is not null) throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "an in-memory database cannot be encrypted");
        if (options.ReadOnly) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "cannot create a read-only database");
        if (options.WorkMem < 0) throw new ArgumentOutOfRangeException(nameof(options));
        var database = await CreateInMemoryAsync(options.PageSize, cancellationToken, options.Environment).ConfigureAwait(false);
        database.SetStorageLimit(options.MaxStorageBytes);
        database.SetWorkMem(options.WorkMem == 0 ? DefaultWorkMem : options.WorkMem);
        return database;
    }

    public static Task<Database> OpenAsync(string path, DatabaseFileOptions? options = null, CancellationToken cancellationToken = default) =>
        OpenFileAsync(path, options, cancellationToken);

    public static Task<Database> DeserializeAsync(ReadOnlyMemory<byte> image, DatabaseDeserializeOptions options, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(options);
        return DeserializeCoreAsync(image, cancellationToken, options.Environment, options.MaxStorageBytes);
    }
}
