namespace Jed;

internal static class CatalogTableCodec
{
    internal static CatalogTable Read(ref WireReader reader, StorageMeta meta, bool encrypted = false)
    {
        var name = Name(ref reader); var columnCount = reader.U16();
        if (columnCount == 0) throw StorageFormat.Corrupt("table has no columns");
        var columns = new CatalogColumn[columnCount]; var names = new HashSet<string>(StringComparer.Ordinal);
        for (var i = 0; i < columns.Length; i++)
        {
            var columnName = Name(ref reader); if (!names.Add(columnName)) throw StorageFormat.Corrupt("duplicate column name");
            var code = reader.Byte(); var flags = reader.Byte();
            if ((flags & 0x81) != 0 || (flags & 12) == 12 || ((flags & 32) != 0 && (flags & 16) == 0) ||
                ((flags & 16) != 0 && (code is < 1 or > 3 || (flags & 10) != 10))) throw StorageFormat.Corrupt("invalid column flags");
            var spec = ReadType(ref reader, code, out var intervalFields);
            if (code == 14 && (flags & ~10) != 0 || code is 15 or 17 && (flags & ~(code == 15 && spec.Type.Element == SqlType.Text ? 66 : 2)) != 0)
                throw StorageFormat.Corrupt("invalid recursive column flags");
            SqlValue? defaultValue = (flags & 4) != 0 ? StoredValues.Decode(ref reader, spec.Type) : null;
            var defaultExpression = (flags & 8) != 0 ? reader.String() : null;
            var collation = (flags & 64) != 0 ? Name(ref reader) : null;
            if (collation is not null && code != 4 && !(code == 15 && spec.Type.Element == SqlType.Text))
                throw StorageFormat.Corrupt("collation on nontext column");
            columns[i] = new(columnName, spec, (flags & 2) != 0, defaultValue, defaultExpression,
                (flags & 16) != 0, (flags & 32) != 0, collation, intervalFields);
        }
        var primaryKey = Ordinals(ref reader, columns.Length, allowEmpty: true, distinct: true);
        var checks = new CatalogCheck[reader.U16()];
        for (var i = 0; i < checks.Length; i++)
        {
            var checkName = Name(ref reader); var expression = reader.String(); var flags = reader.Byte();
            if ((flags & ~1) != 0) throw StorageFormat.Corrupt("invalid check flags");
            checks[i] = new(checkName, expression, (flags & 1) != 0 ? HostDependencies(ref reader) : null);
        }
        var indexes = new CatalogIndex[reader.U16()];
        for (var i = 0; i < indexes.Length; i++)
        {
            var indexName = Name(ref reader); var keys = new CatalogIndexKey[reader.U16()];
            if (keys.Length == 0) throw StorageFormat.Corrupt("index has no keys");
            for (var k = 0; k < keys.Length; k++)
            {
                var ordinal = reader.U16();
                if (ordinal == ushort.MaxValue) keys[k] = new(-1, reader.String());
                else if (ordinal < columnCount) keys[k] = new(ordinal);
                else throw StorageFormat.Corrupt("index column out of bounds");
            }
            var flags = reader.Byte(); var kind = reader.Byte(); var root = reader.U32();
            var rootTag = encrypted ? reader.Bytes(16).ToArray() : [];
            if (encrypted && (root == 0 ? !StorageFormat.IsZero(rootTag) : root < 4)) throw StorageFormat.Corrupt("invalid bound index root");
            if ((flags & 192) != 0 || kind > 2 || (kind != 0 && ((flags & 61) != 0 || keys.Any(key => key.Expression is not null))) ||
                ((flags & 32) != 0 && (flags & 1) == 0) || (kind == 1 && (flags & 2) != 0) ||
                (root != 0 && !StorageFormat.BodyPointer(root, meta.PageCount))) throw StorageFormat.Corrupt("invalid index metadata");
            var predicate = (flags & 2) != 0 ? reader.String() : null;
            var hosts = (flags & 4) != 0 ? HostDependencies(ref reader) : null;
            CatalogTimezoneDependency[]? zones = null; var dynamic = false;
            if ((flags & 8) != 0)
            {
                var dynamicFlag = reader.Byte(); if (dynamicFlag > 1) throw StorageFormat.Corrupt("invalid timezone dependency flags");
                dynamic = dynamicFlag == 1; zones = new CatalogTimezoneDependency[reader.U16()];
                if (!dynamic && zones.Length == 0) throw StorageFormat.Corrupt("empty static timezone dependencies");
                string? prior = null;
                for (var z = 0; z < zones.Length; z++)
                {
                    var zone = Name(ref reader); var version = Name(ref reader); var checksum = reader.U32();
                    if (prior is not null && Utf8NameComparer.Instance.Compare(prior, zone) >= 0) throw StorageFormat.Corrupt("unordered timezone dependencies");
                    prior = zone; zones[z] = new(zone, version, checksum);
                }
            }
            if ((flags & 16) != 0)
            {
                var nonzero = false;
                for (var k = 0; k < keys.Length; k++)
                {
                    var option = reader.Byte(); if ((option & ~3) != 0) throw StorageFormat.Corrupt("reserved index key option");
                    keys[k] = keys[k] with { Options = option }; nonzero |= option != 0;
                }
                if (!nonzero) throw StorageFormat.Corrupt("redundant default index options");
            }
            indexes[i] = new(indexName, keys, kind, root, (flags & 1) != 0, (flags & 32) != 0, predicate, hosts, zones, dynamic) { RootTag = rootTag };
        }
        var foreignKeys = new CatalogForeignKey[reader.U16()];
        for (var i = 0; i < foreignKeys.Length; i++)
        {
            var foreignName = Name(ref reader); var local = Ordinals(ref reader, columnCount, false, false);
            var parent = Name(ref reader); var remote = Ordinals(ref reader, ushort.MaxValue, false, false);
            var actions = reader.Byte();
            if (local.Length != remote.Length || (actions & 128) != 0 || (actions & 7) > 4 || ((actions >> 3) & 7) > 4)
                throw StorageFormat.Corrupt("invalid foreign-key actions");
            foreignKeys[i] = new(foreignName, local, parent, remote, (byte)(actions & 7), (byte)((actions >> 3) & 7), (actions & 64) != 0);
        }
        var exclusions = new CatalogExclusion[reader.U16()];
        for (var i = 0; i < exclusions.Length; i++)
        {
            var exclusionName = Name(ref reader); var indexName = Name(ref reader); var count = reader.U16();
            if (count == 0) throw StorageFormat.Corrupt("exclusion has no elements");
            var elements = new int[count]; var operators = new byte[count];
            for (var e = 0; e < count; e++)
            {
                elements[e] = reader.U16(); operators[e] = reader.Byte();
                if (elements[e] >= columnCount || operators[e] > 1) throw StorageFormat.Corrupt("invalid exclusion element");
            }
            var backing = indexes.FirstOrDefault(index => index.Name == indexName);
            if (backing is null || backing.Unique || backing.Kind == 1 || backing.Kind == 0 && operators.Any(op => op != 1))
                throw StorageFormat.Corrupt("invalid exclusion backing index");
            exclusions[i] = new(exclusionName, indexName, elements, operators);
        }
        foreach (var index in indexes)
            if (index.Kind == 2 && index.Predicate is not null && !exclusions.Any(exclusion => exclusion.Index == index.Name))
                throw StorageFormat.Corrupt("partial GiST index has no exclusion");
        var rootPage = reader.U32(); var tableTag = encrypted ? reader.Bytes(16).ToArray() : []; var rawCount = reader.U64();
        if (encrypted && (rootPage == 0 ? !StorageFormat.IsZero(tableTag) : rootPage < 4)) throw StorageFormat.Corrupt("invalid bound table root");
        if (rawCount > long.MaxValue || (rootPage == 0) != (rawCount == 0) ||
            (rootPage != 0 && !StorageFormat.BodyPointer(rootPage, meta.PageCount)) || rawCount > (ulong)meta.PageCount * (uint)(meta.PageSize - 16))
            throw StorageFormat.Corrupt("invalid table root or row count");
        return new(name, columns, primaryKey, rootPage, (long)rawCount, checks, indexes, foreignKeys, exclusions) { RootTag = tableTag };
    }

    internal static byte[] Write(CatalogTable table, bool encrypted = false)
    {
        var writer = new WireWriter(); writer.Byte(0); writer.String(table.Name); Count(writer, table.Columns.Length);
        foreach (var column in table.Columns)
        {
            writer.String(column.Name); var code = StoredValues.TypeCode(column.Type.Type); writer.Byte(code);
            var flags = (column.NotNull ? 2 : 0) | (column.Default is not null ? 4 : 0) | (column.DefaultExpression is not null ? 8 : 0) |
                (column.Identity ? 16 : 0) | (column.IdentityAlways ? 32 : 0) | (column.Collation is not null ? 64 : 0);
            writer.Byte((byte)flags); WriteType(writer, column.Type, column.IntervalFields);
            if (column.Default is { } value) writer.Bytes(StoredValues.Encode(value));
            if (column.DefaultExpression is { } expression) writer.String(expression);
            if (column.Collation is { } collation) writer.String(collation);
        }
        Ordinals(writer, table.PrimaryKey);
        var checks = (table.Checks ?? []).OrderBy(check => check.Name, Utf8NameComparer.Instance).ToArray(); Count(writer, checks.Length);
        foreach (var check in checks)
        {
            writer.String(check.Name); writer.String(check.Expression); writer.Byte((byte)((check.HostDependencies?.Length ?? 0) > 0 ? 1 : 0));
            if (check.HostDependencies is { Length: > 0 } hosts) HostDependencies(writer, hosts);
        }
        var indexes = (table.Indexes ?? []).OrderBy(index => index.Name, Utf8NameComparer.Instance).ToArray(); Count(writer, indexes.Length);
        foreach (var index in indexes)
        {
            writer.String(index.Name); Count(writer, index.Keys.Length);
            foreach (var key in index.Keys)
            {
                if (key.Expression is { } expression) { writer.U16(ushort.MaxValue); writer.String(expression); }
                else Count(writer, key.Column);
            }
            var flags = (index.Unique ? 1 : 0) | (index.Predicate is not null ? 2 : 0) | ((index.HostDependencies?.Length ?? 0) > 0 ? 4 : 0) |
                (index.TimezoneDependencies is not null ? 8 : 0) | (index.Keys.Any(key => key.Options != 0) ? 16 : 0) | (index.IsConstraint ? 32 : 0);
            writer.Byte((byte)flags); writer.Byte(index.Kind); writer.U32(index.Root);
            if (encrypted) WriteTag(writer, index.Root, index.RootTag.Span);
            if (index.Predicate is { } predicate) writer.String(predicate);
            if ((flags & 4) != 0) HostDependencies(writer, index.HostDependencies!);
            if (index.TimezoneDependencies is { } zones)
            {
                writer.Byte(index.DynamicTimezone ? (byte)1 : (byte)0); Count(writer, zones.Length);
                foreach (var zone in zones.OrderBy(zone => zone.Name, Utf8NameComparer.Instance))
                { writer.String(zone.Name); writer.String(zone.Version); writer.U32(zone.Checksum); }
            }
            if ((flags & 16) != 0) foreach (var key in index.Keys) writer.Byte(key.Options);
        }
        var foreignKeys = (table.ForeignKeys ?? []).OrderBy(key => key.Name, Utf8NameComparer.Instance).ToArray(); Count(writer, foreignKeys.Length);
        foreach (var foreign in foreignKeys)
        {
            writer.String(foreign.Name); Ordinals(writer, foreign.Columns); writer.String(foreign.Parent); Ordinals(writer, foreign.ParentColumns);
            writer.Byte((byte)(foreign.OnDelete | (foreign.OnUpdate << 3) | (foreign.MatchFull ? 64 : 0)));
        }
        var exclusions = (table.Exclusions ?? []).OrderBy(exclusion => exclusion.Name, Utf8NameComparer.Instance).ToArray(); Count(writer, exclusions.Length);
        foreach (var exclusion in exclusions)
        {
            writer.String(exclusion.Name); writer.String(exclusion.Index); Count(writer, exclusion.Columns.Length);
            for (var i = 0; i < exclusion.Columns.Length; i++) { Count(writer, exclusion.Columns[i]); writer.Byte(exclusion.Operators[i]); }
        }
        writer.U32(table.Root); if (encrypted) WriteTag(writer, table.Root, table.RootTag.Span);
        writer.U64(checked((ulong)table.RowCount)); return writer.ToArray();
    }
    private static void WriteTag(WireWriter writer, uint page, ReadOnlySpan<byte> tag)
    {
        if (page == 0) { writer.Bytes(new byte[16]); return; }
        if (page < 4 || tag.Length != 16) throw new ArgumentException("nonzero encrypted catalog root requires a bound tag");
        writer.Bytes(tag);
    }

    internal static TypeSpec ReadType(ref WireReader reader, byte code, out byte intervalFields)
    {
        intervalFields = 0;
        if (code == 14) return new(SqlType.Composite(Name(ref reader)));
        if (code is 15 or 17)
        {
            var elementCode = reader.Byte();
            if (code == 17 && elementCode is not (2 or 3 or 6 or 9 or 10 or 16)) throw StorageFormat.Corrupt("invalid range element type");
            var element = elementCode == 14 && code == 15 ? SqlType.Composite(Name(ref reader)) : StoredValues.FromCode(elementCode);
            var rangeName = elementCode switch { 2 => "i32range", 3 => "i64range", 6 => "numrange", 9 => "tsrange", 10 => "tstzrange", 16 => "daterange", _ => "" };
            return new(code == 15 ? SqlType.Array(element) : SqlType.Range(element, rangeName));
        }
        var type = StoredValues.FromCode(code);
        if (code == 6)
        {
            var precision = reader.U16(); var scale = reader.U16();
            if (precision > 1000 || scale > precision || (precision == 0 && scale != 0)) throw StorageFormat.Corrupt("invalid decimal typmod");
            return precision == 0 ? new(type) : new(type, precision, scale);
        }
        if (code == 4)
        {
            var length = reader.U32(); if (length > 10485760) throw StorageFormat.Corrupt("invalid varchar length");
            return new(type, Length: length == 0 ? null : (int)length);
        }
        if (code is 9 or 10 or 11)
        {
            if (code == 11) { intervalFields = reader.Byte(); if (intervalFields > 13) throw StorageFormat.Corrupt("invalid interval qualifier"); }
            var precision = reader.Byte(); if (precision != 255 && (precision > 6 || code == 11 && intervalFields is not (0 or 6 or 10 or 12 or 13)))
                throw StorageFormat.Corrupt("invalid temporal precision");
            return new(type, precision == 255 ? null : precision, IntervalFields: intervalFields);
        }
        return new(type);
    }

    internal static void WriteType(WireWriter writer, TypeSpec spec, byte intervalFields)
    {
        switch (spec.Type.Kind)
        {
            case SqlKind.Decimal: Count(writer, spec.Precision ?? 0); Count(writer, spec.Scale ?? 0); break;
            case SqlKind.Text: writer.U32((uint)(spec.Length ?? 0)); break;
            case SqlKind.Timestamp: case SqlKind.Timestamptz: writer.Byte((byte)(spec.Precision ?? 255)); break;
            case SqlKind.Interval: writer.Byte(intervalFields); writer.Byte((byte)(spec.Precision ?? 255)); break;
            case SqlKind.Composite: writer.String(spec.Type.Identity!); break;
            case SqlKind.Array: case SqlKind.Range:
                writer.Byte(StoredValues.TypeCode(spec.Type.Element!));
                if (spec.Type.Element!.Kind == SqlKind.Composite) writer.String(spec.Type.Element.Identity!);
                break;
        }
    }

    private static CatalogHostDependency[] HostDependencies(ref WireReader reader)
    {
        var entries = new CatalogHostDependency[reader.U16()];
        if (entries.Length == 0) throw StorageFormat.Corrupt("empty host dependency list");
        for (var i = 0; i < entries.Length; i++)
        {
            var name = Name(ref reader); var arguments = reader.Bytes(reader.U16()).ToArray(); var result = reader.Byte();
            var component = Name(ref reader); var version = reader.U32();
            foreach (var code in arguments.Append(result)) _ = StoredValues.FromCode(code);
            entries[i] = new(name, arguments, result, component, version);
        }
        return entries;
    }
    private static void HostDependencies(WireWriter writer, CatalogHostDependency[] entries)
    {
        Count(writer, entries.Length);
        foreach (var dependency in entries)
        {
            writer.String(dependency.Name); Count(writer, dependency.Arguments.Length); writer.Bytes(dependency.Arguments);
            writer.Byte(dependency.Result); writer.String(dependency.Component); writer.U32(dependency.Version);
        }
    }
    private static int[] Ordinals(ref WireReader reader, int columns, bool allowEmpty, bool distinct)
    {
        var result = new int[reader.U16()];
        if (!allowEmpty && result.Length == 0) throw StorageFormat.Corrupt("empty column list");
        var seen = new HashSet<int>();
        for (var i = 0; i < result.Length; i++)
        {
            result[i] = reader.U16();
            if (result[i] >= columns || distinct && !seen.Add(result[i])) throw StorageFormat.Corrupt("invalid column ordinal");
        }
        return result;
    }
    private static void Ordinals(WireWriter writer, int[] ordinals)
    {
        Count(writer, ordinals.Length); foreach (var ordinal in ordinals) Count(writer, ordinal);
    }
    internal static string Name(ref WireReader reader)
    {
        var name = reader.String();
        if (name.Length == 0 || name.Contains('\0')) throw StorageFormat.Corrupt("invalid empty or NUL catalog name");
        return name;
    }
    internal static void Count(WireWriter writer, int value)
    {
        if (value < 0 || value > ushort.MaxValue) throw Errors.Unsupported("catalog count or ordinal out of range");
        writer.U16((ushort)value);
    }
}
