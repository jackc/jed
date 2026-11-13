namespace Jed;

/// <summary>A reusable parsed statement, independent of the session that prepared it.</summary>
public sealed class PreparedStatement
{
    internal StatementSyntax Syntax { get; }
    internal string Sql { get; }
    internal PreparedStatement(string sql, StatementSyntax syntax) { Sql = sql; Syntax = syntax; }
}

public sealed partial class Session
{
    public PreparedStatement Prepare(string sql)
    {
        using var operation = Enter(); ArgumentNullException.ThrowIfNull(sql);
        if (options.MaxSqlLength > 0 && TextEncoding.ByteCount(sql) > options.MaxSqlLength)
            throw new SqlException(SqlStates.ProgramLimitExceeded, "SQL input exceeds length limit");
        return new(sql, new Parser(sql).ParseStatement());
    }
    public Task<Rows> QueryAsync(PreparedStatement statement, IReadOnlyList<SqlValue>? parameters = null, QueryOptions? options = null, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(statement);
        var query = (options ?? new()) with { Parameters = parameters ?? options?.Parameters ?? [] };
        return QueryCore(statement.Sql, query.MaxCost, cancellationToken, query, statement.Syntax);
    }
    public async Task<long> ExecuteAsync(PreparedStatement statement, IReadOnlyList<SqlValue>? parameters = null, QueryOptions? options = null, CancellationToken cancellationToken = default)
    {
        await using var rows = await QueryAsync(statement, parameters, options, cancellationToken).ConfigureAwait(false);
        await foreach (var _ in ((IAsyncEnumerable<Row>)rows).WithCancellation(cancellationToken).ConfigureAwait(false)) { }
        return rows.AffectedRows;
    }
}
