using System.Buffers.Binary;

namespace Jed;

internal sealed record CatalogColumn(string Name, TypeSpec Type, bool NotNull = false, SqlValue? Default = null,
    string? DefaultExpression = null, bool Identity = false, bool IdentityAlways = false, string? Collation = null,
    byte IntervalFields = 0);
internal sealed record CatalogHostDependency(string Name, byte[] Arguments, byte Result, string Component, uint Version);
internal sealed record CatalogCheck(string Name, string Expression, CatalogHostDependency[]? HostDependencies = null);
internal sealed record CatalogIndexKey(int Column, string? Expression = null, byte Options = 0);
internal sealed record CatalogTimezoneDependency(string Name, string Version, uint Checksum);
internal sealed record CatalogIndex(string Name, CatalogIndexKey[] Keys, byte Kind = 0, uint Root = 0,
    bool Unique = false, bool IsConstraint = false, string? Predicate = null,
    CatalogHostDependency[]? HostDependencies = null, CatalogTimezoneDependency[]? TimezoneDependencies = null,
    bool DynamicTimezone = false)
{
    internal ReadOnlyMemory<byte> RootTag { get; init; }
}
internal sealed record CatalogForeignKey(string Name, int[] Columns, string Parent, int[] ParentColumns,
    byte OnDelete = 0, byte OnUpdate = 0, bool MatchFull = false);
internal sealed record CatalogExclusion(string Name, string Index, int[] Columns, byte[] Operators);
internal sealed record CatalogTable(string Name, CatalogColumn[] Columns, int[] PrimaryKey,
    uint Root = 0, long RowCount = 0, CatalogCheck[]? Checks = null, CatalogIndex[]? Indexes = null,
    CatalogForeignKey[]? ForeignKeys = null, CatalogExclusion[]? Exclusions = null)
{
    internal ReadOnlyMemory<byte> RootTag { get; init; }
    internal StorageLeafSchema LeafSchema()
    {
        var keyOffsets = Enumerable.Repeat(-1, Columns.Length).ToArray(); var offset = 0;
        foreach (var ordinal in PrimaryKey)
        {
            var type = Columns[ordinal].Type.Type.Kind;
            if (type is not (SqlKind.I16 or SqlKind.I32 or SqlKind.I64 or SqlKind.Boolean or SqlKind.Uuid or
                SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz)) break;
            keyOffsets[ordinal] = offset; offset += StoredValues.Width(Columns[ordinal].Type);
        }
        var fixedKey = PrimaryKey.Length == 0 ? 8 : 0;
        if (PrimaryKey.Length != 0)
        {
            foreach (var ordinal in PrimaryKey)
            {
                var type = Columns[ordinal].Type.Type.Kind;
                if (type is not (SqlKind.I16 or SqlKind.I32 or SqlKind.I64 or SqlKind.Boolean or SqlKind.Uuid or
                    SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.F32 or SqlKind.F64 or SqlKind.Interval))
                { fixedKey = 0; break; }
                fixedKey += StoredValues.Width(Columns[ordinal].Type);
            }
            if (fixedKey > ushort.MaxValue) fixedKey = 0;
        }
        return new(Columns.Select((column, index) => new StorageColumn(StoredValues.Width(column.Type), keyOffsets[index],
            column.Type.Type.Kind is SqlKind.I16 or SqlKind.I32 or SqlKind.I64 or SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.Decimal,
            column.Type.Type.Kind is SqlKind.Text or SqlKind.Bytea or SqlKind.Json,
            column.Type.Type.Kind == SqlKind.Array && column.Type.Type.Element!.IsInteger() ? StoredValues.Width(new(column.Type.Type.Element!)) : 0)).ToArray(), fixedKey);
    }

    internal StorageRecord EncodeRow(IReadOnlyList<SqlValue> values, long rowid = 0)
    {
        if (values.Count != Columns.Length) throw new ArgumentException("row width differs from schema");
        var writer = new WireWriter();
        if (PrimaryKey.Length == 0) writer.Bytes(KeyEncoding.Integer(rowid, 8));
        else foreach (var ordinal in PrimaryKey) writer.Bytes(KeyEncoding.Encode(values[ordinal]));
        var cells = new byte[]?[values.Count];
        for (var i = 0; i < cells.Length; i++) cells[i] = StoredValues.Leaf(values[i], Columns[i].Type);
        return new(writer.ToArray(), cells);
    }

    internal SqlValue[] DecodeRow(StorageRecord record)
    {
        if (record.Cells.Length != Columns.Length) throw StorageFormat.Corrupt("row width differs from schema");
        var values = new SqlValue[Columns.Length];
        for (var i = 0; i < values.Length; i++)
            values[i] = record.Cells[i] is { } cell ? StoredValues.FromLeaf(cell.AsMemory(), Columns[i].Type) : SqlValue.Null(Columns[i].Type.Type);
        return values;
    }
}

internal sealed record CatalogWrite(uint Root, int PageCount, uint[] Pages, int PageDelta)
{
    internal ReadOnlyMemory<byte> RootTag { get; init; }
}
internal sealed record Catalog(CatalogTable[] Tables, uint[] Pages, CatalogComposite[]? Types = null,
    CatalogSequence[]? Sequences = null, CatalogCollation[]? Collations = null,
    CatalogStatistic[]? Statistics = null, CatalogView[]? Views = null)
{
    internal Catalog() : this([], []) { }

    internal SqlType? NamedType(string name)
    {
        var composite = (Types ?? []).FirstOrDefault(type => type.Name == name);
        if (composite is null) return null;
        return SqlType.Composite(name, composite.Fields.Select(field => new SqlField(field.Name, field.Type.Type, field.NotNull,
            field.Type.Precision, field.Type.Scale, field.Type.Length, field.IntervalFields)));
    }
    internal Catalog BindTypes()
    {
        var definitions = (Types ?? []).ToDictionary(type => type.Name, StringComparer.Ordinal);
        var resolved = new Dictionary<string, SqlType>(StringComparer.Ordinal);
        SqlType Bind(SqlType type, int depth)
        {
            if (depth > 128) throw StorageFormat.Corrupt("composite dependency nesting limit");
            if (type.Kind == SqlKind.Array) return SqlType.Array(Bind(type.Element!, depth + 1));
            if (type.Kind != SqlKind.Composite) return type;
            var name = type.Identity!;
            if (resolved.TryGetValue(name, out var existing)) return existing;
            if (!definitions.TryGetValue(name, out var definition)) throw StorageFormat.Corrupt("missing composite type");
            var result = SqlType.Composite(name, definition.Fields.Select(field => new SqlField(field.Name, Bind(field.Type.Type, depth + 1), field.NotNull,
                field.Type.Precision, field.Type.Scale, field.Type.Length, field.IntervalFields)));
            resolved.Add(name, result); return result;
        }
        foreach (var name in definitions.Keys) _ = Bind(SqlType.Composite(name), 0);
        return this with
        {
            Types = (Types ?? []).Select(type => type with { Fields = type.Fields.Select(field => field with { Type = field.Type with { Type = Bind(field.Type.Type, 0) } }).ToArray() }).ToArray(),
            Tables = Tables.Select(table => table with { Columns = table.Columns.Select(column => column with { Type = column.Type with { Type = Bind(column.Type.Type, 0) } }).ToArray() }).ToArray()
        };
    }

    internal static ValueTask<Catalog> ReadAsync(StoragePager pager, StorageMeta meta, CancellationToken cancellationToken = default) => ReadAsync(pager, meta, default, cancellationToken);
    internal static ValueTask<Catalog> ReadAsync(StoragePager pager, StorageEncryptedMeta meta, CancellationToken cancellationToken = default) => ReadAsync(pager, meta.Header, meta.RootTag, cancellationToken);
    private static async ValueTask<Catalog> ReadAsync(StoragePager pager, StorageMeta meta, ReadOnlyMemory<byte> rootTag, CancellationToken cancellationToken)
    {
        var builder = new CatalogBuilder(); var pages = new List<uint>(); var seen = new HashSet<uint>();
        var id = meta.Root; var tag = rootTag; var payloadOffset = pager.Encrypted ? 32 : 16;
        var payloadLength = pager.PayloadCapacity - (pager.Encrypted ? 16 : 0);
        async ValueTask<(ReadOnlyMemory<byte> Bytes, StoragePageHeader Header)> Read(uint index)
        {
            if (!StorageFormat.BodyPointer(index, meta.PageCount) || !seen.Add(index)) throw StorageFormat.Corrupt("invalid catalog chain");
            var bytes = await pager.ReadPageAsync(new StoragePageReference(index, tag), cancellationToken).ConfigureAwait(false);
            var header = StorageFormat.ParsePage(bytes.Span, verifyChecksum: !pager.Encrypted);
            if (header.Type != 1) throw StorageFormat.Corrupt("catalog chain points at noncatalog page");
            if (pager.Encrypted)
            {
                tag = bytes.Slice(16, 16);
                if (header.Next == 0 && !StorageFormat.IsZero(tag.Span)) throw StorageFormat.Corrupt("null catalog successor has a tag");
            }
            pages.Add(index); return (bytes, header);
        }
        while (id != 0)
        {
            var (bytes, header) = await Read(id).ConfigureAwait(false); id = header.Next;
            if ((header.Flags & 1) == 0) { ParseEntries(bytes.Span.Slice(payloadOffset, payloadLength), header.Count, meta, builder, pager.Encrypted); continue; }
            if (header.Count != 1) throw StorageFormat.Corrupt("continued catalog entry has wrong count");
            using var entry = new MemoryStream(); entry.Write(bytes.Span.Slice(payloadOffset, payloadLength));
            do
            {
                if (id == 0) throw StorageFormat.Corrupt("unfinished catalog continuation");
                (bytes, header) = await Read(id).ConfigureAwait(false); id = header.Next;
                if (header.Count != 0) throw StorageFormat.Corrupt("catalog continuation starts another entry");
                entry.Write(bytes.Span.Slice(payloadOffset, payloadLength));
            } while ((header.Flags & 1) != 0);
            ParseEntries(entry.GetBuffer().AsSpan(0, (int)entry.Length), 1, meta, builder, pager.Encrypted);
        }
        if (pages.Count == 0) throw StorageFormat.Corrupt("missing catalog chain");
        var catalog = new Catalog(builder.Tables.ToArray(), pages.ToArray(), builder.Types.ToArray(),
            builder.Sequences.ToArray(), builder.Collations.ToArray(), builder.Statistics.ToArray(), builder.Views.ToArray());
        catalog.Validate();
        try { return catalog.BindTypes(); }
        catch (SqlException error) when (error.SqlState == SqlStates.ProgramLimitExceeded) { throw StorageFormat.Corrupt("composite nesting exceeds format limit"); }
    }

    private static void ParseEntries(ReadOnlySpan<byte> payload, uint count, StorageMeta meta, CatalogBuilder builder, bool encrypted)
    {
        if (count > payload.Length) throw StorageFormat.Corrupt("catalog item count out of bounds");
        var reader = new WireReader(payload);
        for (uint i = 0; i < count; i++)
        {
            var kind = reader.Byte();
            if (kind == 0) builder.Tables.Add(CatalogTableCodec.Read(ref reader, meta, encrypted));
            else CatalogObjectCodec.Read(kind, ref reader, builder);
        }
        if (!StorageFormat.IsZero(reader.Bytes(reader.Remaining))) throw StorageFormat.Corrupt("nonzero catalog padding");
    }

    internal CatalogWrite Serialize(StorageAllocation allocation)
    {
        Validate();
        var capacity = allocation.PayloadCapacity - (allocation.Encrypted ? 16 : 0);
        var chunks = new List<(byte[] Payload, uint Count, byte Flags)>();
        using var current = new MemoryStream(); uint count = 0;
        void Flush()
        {
            if (count == 0) return;
            chunks.Add((current.ToArray(), count, 0)); current.SetLength(0); count = 0;
        }
        foreach (var entry in CatalogObjectCodec.Entries(this, allocation.Encrypted))
        {
            if (entry.Length > capacity)
            {
                Flush();
                for (var at = 0; at < entry.Length; at += capacity)
                {
                    var n = Math.Min(capacity, entry.Length - at);
                    chunks.Add((entry.AsSpan(at, n).ToArray(), at == 0 ? 1u : 0u, (byte)(at + n < entry.Length ? 1 : 0)));
                }
            }
            else
            {
                if (current.Length + entry.Length > capacity) Flush();
                current.Write(entry); count++;
            }
        }
        Flush();
        if (chunks.Count == 0) chunks.Add(([], 0, 0));
        foreach (var page in Pages) allocation.Reclaim(page);
        var ids = new uint[chunks.Count];
        for (var i = 0; i < ids.Length; i++) ids[i] = allocation.ReservePage();
        for (var i = ids.Length - 1; i >= 0; i--)
        {
            var chunk = chunks[i];
            var payload = chunk.Payload;
            if (allocation.Encrypted)
            {
                payload = new byte[16 + chunk.Payload.Length];
                if (i + 1 < ids.Length) allocation.Tag(ids[i + 1]).Span.CopyTo(payload);
                chunk.Payload.CopyTo(payload, 16);
            }
            allocation.SetPage(ids[i], StorageFormat.EncodePage(allocation.PageSize,
                new(1, chunk.Flags, chunk.Count, i + 1 < ids.Length ? ids[i + 1] : 0), payload));
        }
        return new(ids[0], ids.Length, ids, ids.Length - Pages.Length) { RootTag = allocation.Tag(ids[0]) };
    }

    private void Validate()
    {
        CatalogValidation.Validate(this);
        var tables = new Dictionary<string, CatalogTable>(StringComparer.Ordinal);
        foreach (var table in Tables)
            if (!tables.TryAdd(table.Name, table)) throw StorageFormat.Corrupt("duplicate table name");
        foreach (var table in tables.Values)
            foreach (var foreign in table.ForeignKeys ?? [])
            {
                if (!tables.TryGetValue(foreign.Parent, out var parent) || foreign.ParentColumns.Length != foreign.Columns.Length)
                    throw StorageFormat.Corrupt("missing foreign-key target");
                for (var i = 0; i < foreign.Columns.Length; i++)
                    if (foreign.ParentColumns[i] < 0 || foreign.ParentColumns[i] >= parent.Columns.Length ||
                        !CatalogValidation.ForeignKeyTypes(table.Columns[foreign.Columns[i]].Type.Type, parent.Columns[foreign.ParentColumns[i]].Type.Type))
                        throw StorageFormat.Corrupt("foreign-key target column mismatch");
            }
    }
}

internal sealed class Utf8NameComparer : IComparer<string>
{
    internal static readonly Utf8NameComparer Instance = new();
    public int Compare(string? left, string? right) => TextEncoding.Utf8.GetBytes(left ?? "").AsSpan().SequenceCompareTo(TextEncoding.Utf8.GetBytes(right ?? ""));
}
