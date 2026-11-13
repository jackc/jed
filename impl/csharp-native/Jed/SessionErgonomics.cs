namespace Jed;

public sealed partial class Rows
{
    public int GetOrdinal(string name)
    {
        ArgumentNullException.ThrowIfNull(name);
        for (var i = 0; i < Columns.Count; i++) if (Columns[i].Name == name) return i;
        for (var i = 0; i < Columns.Count; i++) if (StringComparer.OrdinalIgnoreCase.Equals(Columns[i].Name, name)) return i;
        throw new SqlException(SqlStates.UndefinedColumn, "result column does not exist: " + name);
    }
}

public sealed partial class Session
{
    /// <summary>Native values can use SqlValue's implicit conversions, for example [42, "text"].</summary>
    public Task<Rows> QueryAsync(string sql, IReadOnlyList<SqlValue> parameters, QueryOptions? options = null, CancellationToken cancellationToken = default) =>
        QueryAsync(sql, (options ?? new()) with { Parameters = parameters }, cancellationToken);

    public async Task<long?> ExecuteAsync(string sql, IReadOnlyList<SqlValue>? parameters = null, QueryOptions? options = null, CancellationToken cancellationToken = default)
    {
        await using var rows = await QueryAsync(sql, parameters ?? options?.Parameters ?? [], options, cancellationToken).ConfigureAwait(false);
        await foreach (var _ in ((IAsyncEnumerable<Row>)rows).WithCancellation(cancellationToken).ConfigureAwait(false)) { }
        return rows.AffectedRows;
    }

    /// <summary>Reads the first row and closes the cursor; returns null when there are no rows.</summary>
    public async Task<Row?> QueryFirstOrDefaultAsync(string sql, IReadOnlyList<SqlValue>? parameters = null, QueryOptions? options = null, CancellationToken cancellationToken = default)
    {
        await using var rows = await QueryAsync(sql, parameters ?? options?.Parameters ?? [], options, cancellationToken).ConfigureAwait(false);
        return await rows.ReadAsync(cancellationToken).ConfigureAwait(false);
    }

    /// <summary>Maps streaming rows without reflection or an engine-owned result collector.</summary>
    public async IAsyncEnumerable<T> QueryAsync<T>(string sql, Func<Row, T> map, IReadOnlyList<SqlValue>? parameters = null,
        QueryOptions? options = null, [System.Runtime.CompilerServices.EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(map);
        await using var rows = await QueryAsync(sql, parameters ?? options?.Parameters ?? [], options, cancellationToken).ConfigureAwait(false);
        await foreach (var row in ((IAsyncEnumerable<Row>)rows).WithCancellation(cancellationToken).ConfigureAwait(false)) yield return map(row);
    }
}
