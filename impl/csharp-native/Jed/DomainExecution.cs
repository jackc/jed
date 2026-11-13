namespace Jed;

internal static class DomainExecution
{
    internal static string? Target(StatementSyntax statement) => statement switch
    {
        ReindexSyntax { Kind: "index" or "table" } value => value.Name,
        AnalyzeSyntax value => value.Table,
        InsertSyntax value => value.Table, UpdateSyntax value => value.Table, DeleteSyntax value => value.Table, MergeSyntax value => value.Table,
        CreateTableSyntax value => value.Name, CreateTableAsSyntax value => value.Name, DropTableSyntax value => value.Name, DropTablesSyntax value => value.Names[0],
        CreateIndexSyntax value => value.Table, DropIndexSyntax value => value.Name,
        AlterTableSyntax value => value.Table, AlterBatchSyntax value => value.Actions[0].Table,
        CreateViewSyntax value => value.Name, AlterViewSyntax value => value.Name, DropViewSyntax value => value.Names[0],
        CreateSequenceSyntax value => value.Name, AlterSequenceSyntax value => value.Name, DropSequenceSyntax value => value.Names[0],
        CreateTypeSyntax value => value.Name, DropTypeSyntax value => value.Name, _ => null
    };
    internal static bool Creation(StatementSyntax statement) => statement is CreateTableSyntax or CreateTableAsSyntax or CreateViewSyntax or CreateSequenceSyntax or CreateTypeSyntax;
    internal static bool Temporary(StatementSyntax statement) => statement is CreateTableSyntax { Temporary: true } or CreateTableAsSyntax { Temporary: true } or CreateViewSyntax { Temporary: true };
    internal static DatabaseDomain ResolveTarget(StatementSyntax statement, DatabaseDomains domains)
    {
        var target = Target(statement)!;
        if (statement is CreateViewSyntax view && DatabaseDomains.Name(target).Domain is null)
        {
            if (view.Replace && (domains.Get("temp").Catalog.Views ?? []).Any(item => item.Name == target)) return domains.Get("temp");
            if (!view.Temporary)
            {
                var dependencies = StatementExecution.ViewDependencies(view.Query, domains.BindCatalog(domains.Get("main")));
                var temp = domains.Get("temp").Catalog;
                if (dependencies.Any(dependency => dependency.Kind switch
                {
                    0 => temp.Tables.Any(table => table.Name == dependency.Name),
                    1 => (temp.Views ?? []).Any(item => item.Name == dependency.Name),
                    2 => (temp.Sequences ?? []).Any(sequence => sequence.Name == dependency.Name), _ => false
                })) return domains.Get("temp");
            }
        }
        return domains.Resolve(target, Creation(statement), Temporary(statement));
    }
    internal static StatementSyntax Local(StatementSyntax statement, string domain, DatabaseDomains? domains = null)
    {
        string Name(string value)
        {
            var parts = DatabaseDomains.Name(value);
            if (parts.Domain is { } qualifier && qualifier != domain) throw Errors.Unsupported("cross-domain stored reference");
            return parts.Local;
        }
        ConstraintSyntax Constraint(ConstraintSyntax value)
        {
            if (value.Parent is not { } parent) return value;
            if (domains is null) return value with { Parent = Name(parent) };
            var parts = DatabaseDomains.Name(parent);
            var home = domains.Resolve(parent);
            if (domain is not ("main" or "temp") || home.Name is not ("main" or "temp"))
                throw Errors.Unsupported("foreign keys on attached databases");
            var self = parts.Local == DatabaseDomains.Name(Target(statement)!).Local && (parts.Domain is null || parts.Domain == domain);
            if (!self && !home.Catalog.Tables.Any(table => table.Name == parts.Local))
                throw new SqlException(SqlStates.UndefinedTable, "foreign key parent does not exist");
            if (!self && home.Name != domain) throw new SqlException(SqlStates.InvalidTableDefinition, "foreign key crosses temporary and persistent domains");
            return value with { Parent = parts.Local };
        }
        AlterTableSyntax Alter(AlterTableSyntax value) => value with { Table = Name(value.Table), Constraint = value.Constraint is null ? null : Constraint(value.Constraint), AddedConstraints = value.AddedConstraints?.Select(Constraint).ToArray() };
        return statement switch
        {
            ReindexSyntax { Kind: "index" or "table" } value => value with { Name = Name(value.Name!) },
            AnalyzeSyntax value => value with { Table = Name(value.Table) },
            InsertSyntax value => value with { Table = Name(value.Table) }, UpdateSyntax value => value with { Table = Name(value.Table) }, DeleteSyntax value => value with { Table = Name(value.Table) }, MergeSyntax value => value with { Table = Name(value.Table) },
            CreateTableSyntax value => value with { Name = Name(value.Name), Constraints = value.Constraints?.Select(Constraint).ToArray() },
            CreateTableAsSyntax value => value with { Name = Name(value.Name) }, DropTableSyntax value => value with { Name = Name(value.Name) }, DropTablesSyntax value => value with { Names = value.Names.Select(Name).ToArray() },
            CreateIndexSyntax value => value with { Table = Name(value.Table), Name = value.Name is null ? null : Name(value.Name) }, DropIndexSyntax value => value with { Name = Name(value.Name) },
            AlterTableSyntax value => Alter(value), AlterBatchSyntax value => value with { Actions = value.Actions.Select(Alter).ToArray() },
            CreateViewSyntax value => value with { Name = Name(value.Name) }, AlterViewSyntax value => value with { Name = Name(value.Name) }, DropViewSyntax value => value with { Names = value.Names.Select(Name).ToArray() },
            CreateSequenceSyntax value => value with { Name = Name(value.Name) }, AlterSequenceSyntax value => value with { Name = Name(value.Name) }, DropSequenceSyntax value => value with { Names = value.Names.Select(Name).ToArray() },
            CreateTypeSyntax value => value with { Name = Name(value.Name) }, DropTypeSyntax value => value with { Name = Name(value.Name) }, _ => statement
        };
    }
    internal static BoundColumn[] Infer(Parameters parameters, StatementSyntax statement, Catalog catalog, Meter meter, CancellationToken token, QueryScope? queryScope)
    {
        var domains = catalog.Scope!; var target = Target(statement)!;
        var domain = ResolveTarget(statement, domains);
        return parameters.Infer(Local(statement, domain.Name, domains), domain.Database, domain.Snapshot, domain.Pending, domains.BindCatalog(domain), meter, token, queryScope, localDomain: true);
    }
    internal static PendingDatabase ReadPending(DatabaseDomains domains, string name)
    {
        var domain = domains.Get(name);
        var pending = domain.Pending ?? new PendingDatabase(domain.Snapshot);
        pending.Catalog = domains.BindCatalog(domain); return pending;
    }
}

internal static partial class StatementExecution
{
    private static async ValueTask<StatementResult> ExecuteDomain(Database database, PendingDatabase pending, StatementSyntax statement,
        Meter meter, CancellationToken token, StatementContext? context)
    {
        var domains = pending.Catalog.Scope!; var target = DomainExecution.Target(statement);
        if (target is null) return await ExecuteLocal(database, pending, statement, meter, token, context).ConfigureAwait(false);
        var domain = DomainExecution.ResolveTarget(statement, domains);
        if (domain.ReadOnly || domain.Pending is null) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "database domain is read-only");
        var alreadyExists = RelationExists(domain.Catalog, DatabaseDomains.Name(target).Local);
        if (alreadyExists && statement is CreateTableSyntax { IfNotExists: true } or CreateTableAsSyntax { IfNotExists: true }) return new([], []);
        if (domain.Name is not ("main" or "temp"))
        {
            if (statement is CreateTableSyntax create && create.Columns.Any(column => column.Identity || column.Type is "serial" or "smallserial" or "bigserial" or "serial2" or "serial4" or "serial8"))
                throw Errors.Unsupported("serial and identity columns on attached databases");
            if (statement is InsertSyntax { Conflict: not null }) throw Errors.Unsupported("ON CONFLICT on attached databases");
            if (statement is CreateViewSyntax or AlterViewSyntax or DropViewSyntax) throw Errors.Unsupported("views in attached databases");
        }
        if (domain.Name == "temp" && statement is CreateIndexSyntax { Method: "gist" }) throw Errors.Unsupported("GiST index on temporary table");
        var local = DomainExecution.Local(statement, domain.Name, domains);
        if (DomainExecution.Creation(statement) && domain.Name is "main" or "temp")
        {
            var name = DatabaseDomains.Name(target).Local;
            var collision = domains.Items.Where(item => item.Name is "main" or "temp" && item.Name != domain.Name).Any(item => RelationExists(item.Catalog, name));
            if (collision)
            {
                if (statement is CreateTableSyntax { IfNotExists: true } or CreateTableAsSyntax { IfNotExists: true } or CreateViewSyntax { IfNotExists: true } or CreateSequenceSyntax { IfNotExists: true }) return new([], []);
                throw new SqlException(SqlStates.DuplicateTable, "relation already exists in the implicit namespace");
            }
        }
        var ownContext = context is null;
        context ??= new(DomainExecution.ReadPending(domains.Fork(), domain.Name), new QueryScope());
        context = context.ForDomain(domain.Name);
        domain.Pending.Catalog = domains.BindCatalog(domain);
        using var sequenceBinding = meter.Sequences.PushBinding(domain.Pending.Catalog with { Scope = domains.Stored(domain.Name) }, domain.Pending);
        var existed = DomainExecution.Creation(statement) && RelationExists(domain.Catalog, DatabaseDomains.Name(target).Local);
        var result = await ExecuteLocal(domain.Database, domain.Pending, local, meter, token, context).ConfigureAwait(false);
        if (!existed && domain.Name == "temp" && local is CreateTableSyntax or CreateTableAsSyntax)
        {
            var name = DomainExecution.Target(local)!;
            var onCommit = local is CreateTableSyntax table ? table.OnCommit : ((CreateTableAsSyntax)local).OnCommit;
            domain.Pending.Catalog = domain.Pending.Catalog with { Tables = domain.Pending.Catalog.Tables.Select(table => table.Name == name ? table with { OnCommit = onCommit } : table).ToArray() };
        }
        if (ownContext) await context.Settle().ConfigureAwait(false);
        return result;
    }
}
