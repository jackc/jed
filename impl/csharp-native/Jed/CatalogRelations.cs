using System.Runtime.CompilerServices;

namespace Jed;

internal static class CatalogRelations
{
    internal static bool IsCatalogName(string name) => DatabaseDomains.Name(name).Local is "jed_tables" or "jed_columns" or "jed_indexes" or "jed_constraints" or "jed_sequences" or "jed_types" or "jed_type_fields" or "jed_views" or "jed_statistics";
    internal static PreparedRelation Prepare(TableRelation relation, Database database, DatabaseSnapshot snapshot, PendingDatabase? pending,
        Catalog catalog, Meter meter, CancellationToken token)
    {
        var (qualifier, name) = DatabaseDomains.Name(relation.Name);
        if (catalog.Scope is { } scope)
        {
            var domain = scope.Get(qualifier ?? "main");
            database = domain.Database; snapshot = domain.Snapshot; pending = domain.Pending;
            catalog = domain.Catalog with { Scope = scope.Stored(domain.Name) };
        }
        else if (qualifier is not (null or "main")) throw new SqlException(SqlStates.UndefinedTable, "database is not attached: " + qualifier);
        BoundColumn[] Schema(params (string Name, SqlType Type, bool Null)[] fields) => fields.Select(field => new BoundColumn(field.Name, field.Type, name,
            NotNull: !field.Null, Collation: CollationRules.Collatable(field.Type) ? new("C") : null)).ToArray();
        var text = SqlType.Text; var integer = SqlType.I32; var wide = SqlType.I64; var boolean = SqlType.Boolean;
        var texts = SqlType.Array(text); var bools = SqlType.Array(boolean);
        var columns = name switch
        {
            "jed_tables" => Schema(("name", text, false)),
            "jed_columns" => Schema(("table_name", text, false), ("name", text, false), ("ordinal", integer, false), ("type", text, false), ("not_null", boolean, false), ("pk_ordinal", integer, true), ("column_default", text, true), ("identity_generation", text, true)),
            "jed_indexes" => Schema(("name", text, false), ("table_name", text, false), ("columns", texts, false), ("is_unique", boolean, false), ("method", text, false), ("predicate", text, true), ("descending", bools, false), ("nulls_first", bools, false)),
            "jed_constraints" => Schema(("name", text, false), ("table_name", text, false), ("type", text, false), ("columns", texts, true), ("expression", text, true), ("ref_table", text, true), ("ref_columns", texts, true), ("operators", texts, true), ("match_type", text, true)),
            "jed_sequences" => Schema(("name", text, false), ("start_value", wide, false), ("min_value", wide, false), ("max_value", wide, false), ("increment_by", wide, false), ("cycle", boolean, false), ("cache_size", wide, false), ("last_value", wide, true), ("owned_by_table", text, true), ("owned_by_column", text, true)),
            "jed_types" => Schema(("name", text, false), ("kind", text, false)),
            "jed_type_fields" => Schema(("type_name", text, false), ("name", text, false), ("ordinal", integer, false), ("type", text, false), ("not_null", boolean, false)),
            "jed_views" => Schema(("name", text, false), ("definition", text, false), ("columns", texts, false), ("check_option", text, false), ("security_barrier", boolean, false), ("security_invoker", boolean, false)),
            "jed_statistics" => Schema(("table_name", text, false), ("column_name", text, false), ("analyzed_rows", wide, false), ("is_stale", boolean, false), ("null_count", wide, false), ("distinct_count", wide, true), ("sample_rows", wide, false), ("average_width", wide, true), ("mcv_count", integer, false), ("histogram_count", integer, false), ("term_count", integer, true)),
            _ => throw new InvalidOperationException("unknown catalog relation")
        };
        var context = new QueryStorage(database, snapshot, pending, catalog);
        return new(RelationExecution.Alias(columns, relation.Alias ?? name, relation.ColumnAliases), Rows(name, context, meter, token));
    }
    private static SqlValue T(string? value) => value is null ? SqlValue.Null(SqlType.Text) : SqlValue.Text(value);
    private static SqlValue I(long value) => SqlValue.Integer(value, SqlType.I32);
    private static SqlValue L(long value) => SqlValue.Integer(value, SqlType.I64);
    private static SqlValue B(bool value) => SqlValue.Boolean(value);
    private static SqlValue A(IEnumerable<string>? values) => values is null ? SqlValue.Null(SqlType.Array(SqlType.Text)) : SqlValue.Array(new(SqlType.Text, values.Select(T)));
    private static SqlValue BA(IEnumerable<bool> values) => SqlValue.Array(new(SqlType.Boolean, values.Select(B)));
    private static string TypeName(TypeSpec type, byte intervalFields = 0)
    {
        var name = type.Type.Name();
        if (type.Type == SqlType.Text && type.Length is { } length) return $"varchar({length})";
        if (type.Type == SqlType.Decimal && type.Precision is { } precision) return $"decimal({precision},{type.Scale ?? 0})";
        if (type.Type == SqlType.Interval && (intervalFields != 0 || type.IntervalFields != 0))
            name += " " + DatetimeUnits.Qualifiers.First(item => item.Code == (intervalFields == 0 ? type.IntervalFields : intervalFields)).Name;
        if (type.Type.Kind is SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.Interval && type.Precision is { } temporal) name += $"({temporal})";
        return name;
    }
    private static string? Default(CatalogColumn column)
    {
        if (column.DefaultExpression is { } expression) return expression;
        if (column.Default is not { IsNull: false } value) return null;
        var text = value.ToString();
        return value.Type.IsInteger() || value.Type == SqlType.Decimal || value.Type == SqlType.Boolean ? text : "'" + text.Replace("'", "''", StringComparison.Ordinal) + "'";
    }
    private static IEnumerable<SqlValue[]> Generate(string name, QueryStorage context, Meter meter, CancellationToken token)
    {
        var catalog = context.Catalog;
        var tables = catalog.Tables.OrderBy(table => table.Name, Utf8NameComparer.Instance);
        switch (name)
        {
            case "jed_tables": foreach (var table in tables) yield return [T(table.Name)]; break;
            case "jed_columns":
                foreach (var table in tables)
                    for (var i = 0; i < table.Columns.Length; i++)
                    {
                        var column = table.Columns[i]; var pk = Array.IndexOf(table.PrimaryKey, i);
                        yield return [T(table.Name), T(column.Name), I(i + 1), T(TypeName(column.Type, column.IntervalFields)), B(column.NotNull || pk >= 0), pk < 0 ? SqlValue.Null(SqlType.I32) : I(pk + 1), T(Default(column)), T(column.Identity ? column.IdentityAlways ? "ALWAYS" : "BY DEFAULT" : null)];
                    }
                foreach (var view in (catalog.Views ?? []).OrderBy(view => view.Name, Utf8NameComparer.Instance))
                {
                    var query = StatementExecution.ViewQuery(view);
                    var output = RelationExecution.Query(query, context.Database, context.Snapshot, context.Pending, catalog, meter, token);
                    for (var i = 0; i < output.Columns.Length; i++)
                        yield return [T(view.Name), T(view.Columns[i].Name), I(i + 1), T(TypeName(StatementExecution.CatalogQueryColumnType(query, output.Columns[i], i, catalog))), B(false), SqlValue.Null(SqlType.I32), T(view.Columns[i].DefaultExpression), T(null)];
                }
                break;
            case "jed_indexes":
                foreach (var item in tables.SelectMany(table => (table.Indexes ?? []).Select(index => (table, index))).OrderBy(item => item.index.Name, Utf8NameComparer.Instance))
                {
                    var (table, index) = item;
                    yield return [T(index.Name), T(table.Name), A(index.Keys.Select(key => key.Expression ?? table.Columns[key.Column].Name)), B(index.Unique), T(index.Kind switch { 0 => "btree", 1 => "gin", _ => "gist" }), T(index.Predicate), BA(index.Keys.Select(key => (key.Options & 1) != 0)), BA(index.Keys.Select(key => (key.Options & 2) != 0))];
                }
                break;
            case "jed_constraints":
                foreach (var table in tables)
                {
                    var rows = new List<SqlValue[]>();
                    foreach (var check in table.Checks ?? []) rows.Add([T(check.Name), T(table.Name), T("check"), A(null), T(check.Expression), T(null), A(null), A(null), T(null)]);
                    foreach (var index in table.Indexes ?? [])
                        if (index.Unique && index.IsConstraint) rows.Add([T(index.Name), T(table.Name), T("unique"), A(index.Keys.Select(key => table.Columns[key.Column].Name)), T(null), T(null), A(null), A(null), T(null)]);
                    foreach (var foreign in table.ForeignKeys ?? [])
                    {
                        var parent = catalog.Tables.Single(table => table.Name == foreign.Parent);
                        rows.Add([T(foreign.Name), T(table.Name), T("foreign_key"), A(foreign.Columns.Select(i => table.Columns[i].Name)), T(null), T(parent.Name), A(foreign.ParentColumns.Select(i => parent.Columns[i].Name)), A(null), T(foreign.MatchFull ? "full" : "simple")]);
                    }
                    foreach (var exclusion in table.Exclusions ?? []) rows.Add([T(exclusion.Name), T(table.Name), T("exclude"), A(exclusion.Columns.Select(i => table.Columns[i].Name)), T(table.Indexes!.Single(index => index.Name == exclusion.Index).Predicate), T(null), A(null), A(exclusion.Operators.Select(op => op == 0 ? "&&" : "=")), T(null)]);
                    foreach (var row in rows.OrderBy(row => row[0].AsText(), Utf8NameComparer.Instance)) yield return row;
                }
                break;
            case "jed_sequences":
                foreach (var sequence in (catalog.Sequences ?? []).OrderBy(sequence => sequence.Name, Utf8NameComparer.Instance))
                    yield return [T(sequence.Name), L(sequence.Start), L(sequence.Minimum), L(sequence.Maximum), L(sequence.Increment), B(sequence.Cycle), L(sequence.Cache), sequence.IsCalled ? L(sequence.LastValue) : SqlValue.Null(SqlType.I64), T(sequence.OwnerTable), T(sequence.OwnerTable is null ? null : catalog.Tables.Single(table => table.Name == sequence.OwnerTable).Columns[sequence.OwnerColumn].Name)];
                break;
            case "jed_types": foreach (var type in (catalog.Types ?? []).OrderBy(type => type.Name, Utf8NameComparer.Instance)) yield return [T(type.Name), T("composite")]; break;
            case "jed_type_fields":
                foreach (var type in (catalog.Types ?? []).OrderBy(type => type.Name, Utf8NameComparer.Instance))
                    for (var i = 0; i < type.Fields.Length; i++)
                    { var field = type.Fields[i]; yield return [T(type.Name), T(field.Name), I(i + 1), T(TypeName(field.Type, field.IntervalFields)), B(field.NotNull)]; }
                break;
            case "jed_views":
                foreach (var view in (catalog.Views ?? []).OrderBy(view => view.Name, Utf8NameComparer.Instance))
                    yield return [T(view.Name), T(view.Definition), A(view.Columns.Select(column => column.Name)), T(view.CheckOption switch { 1 => "LOCAL", 2 => "CASCADED", _ => "NONE" }), B(view.SecurityBarrier), B(view.SecurityInvoker)];
                break;
            case "jed_statistics":
                foreach (var statistic in (catalog.Statistics ?? []).Where(stat => stat.Kind == 0).OrderBy(stat => stat.Table, Utf8NameComparer.Instance).ThenBy(stat => stat.Column))
                    yield return [T(statistic.Table), T(catalog.Tables.Single(table => table.Name == statistic.Table).Columns[statistic.Column].Name), L(statistic.AnalyzedRows), B((statistic.Flags & 1) != 0), L(statistic.NullCount), (statistic.Flags & 2) != 0 ? L(statistic.DistinctCount) : SqlValue.Null(SqlType.I64), L(statistic.SampleRows), statistic.AnalyzedRows == statistic.NullCount ? SqlValue.Null(SqlType.I64) : L(statistic.WidthSum / (statistic.AnalyzedRows - statistic.NullCount) + (statistic.WidthSum % (statistic.AnalyzedRows - statistic.NullCount) == 0 ? 0 : 1)), I(statistic.McvCount), I(statistic.HistogramCount), (statistic.Flags & 4) != 0 ? I(statistic.TermCount) : SqlValue.Null(SqlType.I32)];
                break;
        }
    }
    private static async IAsyncEnumerable<SqlValue[]> Rows(string name, QueryStorage context, Meter meter, [EnumeratorCancellation] CancellationToken token)
    {
        await ValueTask.CompletedTask.ConfigureAwait(false);
        foreach (var row in Generate(name, context, meter, token))
        { meter.Charge(Costs.GeneratedRow); meter.Checkpoint(token); yield return row; }
    }
}

internal static partial class StatementExecution
{
    internal static TypeSpec CatalogQueryColumnType(SelectSyntax query, BoundColumn column, int index, Catalog catalog) => QueryColumnType(query, column, index, catalog);
}
