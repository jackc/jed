namespace Jed;

internal sealed record CatalogField(string Name, TypeSpec Type, bool NotNull = false, byte IntervalFields = 0);
internal sealed record CatalogComposite(string Name, CatalogField[] Fields);
internal sealed record CatalogSequence(string Name, long Increment, long Minimum, long Maximum, long Start, long Cache,
    long LastValue, bool Cycle = false, bool IsCalled = false, string? OwnerTable = null, int OwnerColumn = 0);
internal sealed record CatalogCollation(string Name, string UnicodeVersion, string CldrVersion, string Description, bool IsDefault = false);
internal sealed record CatalogViewColumn(string Name, string? DefaultExpression = null);
internal sealed record CatalogViewDependency(byte Kind, string Name, string[] Columns);
internal sealed record CatalogView(string Name, CatalogViewColumn[] Columns, string Definition, CatalogViewDependency[] Dependencies,
    byte CheckOption = 0, bool SecurityBarrier = false, bool SecurityInvoker = false);
internal sealed record CatalogStatistic(byte Kind, string Table, int Column, byte Flags = 0, long AnalyzedRows = 0,
    long NullCount = 0, long WidthSum = 0, long DistinctCount = 0, uint SampleRows = 0, uint SampleNonnullRows = 0,
    int McvCount = 0, int HistogramCount = 0, long TermOccurrences = 0, long UntrackedTermRows = 0, int TermCount = 0,
    int ItemOrdinal = 0, uint SampleFrequency = 0, long RowCount = 0, byte[]? Value = null);

internal sealed class CatalogBuilder
{
    internal readonly List<CatalogTable> Tables = [];
    internal readonly List<CatalogComposite> Types = [];
    internal readonly List<CatalogSequence> Sequences = [];
    internal readonly List<CatalogCollation> Collations = [];
    internal readonly List<CatalogStatistic> Statistics = [];
    internal readonly List<CatalogView> Views = [];
}

internal static class CatalogObjectCodec
{
    internal static void Read(byte kind, ref WireReader reader, CatalogBuilder builder)
    {
        switch (kind)
        {
            case 1:
            {
                var name = CatalogTableCodec.Name(ref reader); var fields = new CatalogField[reader.U16()];
                if (fields.Length == 0) throw StorageFormat.Corrupt("empty composite type");
                var names = new HashSet<string>(StringComparer.Ordinal);
                for (var i = 0; i < fields.Length; i++)
                {
                    var field = CatalogTableCodec.Name(ref reader); if (!names.Add(field)) throw StorageFormat.Corrupt("duplicate composite field");
                    var code = reader.Byte(); TypeSpec? type = null; byte interval = 0;
                    if (code is 14 or 15 or 17) type = CatalogTableCodec.ReadType(ref reader, code, out interval);
                    var flags = reader.Byte(); if ((flags & ~1) != 0) throw StorageFormat.Corrupt("invalid composite field flags");
                    type ??= CatalogTableCodec.ReadType(ref reader, code, out interval);
                    fields[i] = new(field, type, (flags & 1) != 0, interval);
                }
                builder.Types.Add(new(name, fields)); break;
            }
            case 2:
            {
                var name = CatalogTableCodec.Name(ref reader);
                var increment = Signed(ref reader); var minimum = Signed(ref reader); var maximum = Signed(ref reader);
                var start = Signed(ref reader); var cache = Signed(ref reader); var last = Signed(ref reader); var flags = reader.Byte();
                if ((flags & ~7) != 0 || increment == 0 || minimum > maximum || start < minimum || start > maximum ||
                    last < minimum || last > maximum || cache < 1) throw StorageFormat.Corrupt("invalid sequence parameters");
                var owner = (flags & 4) != 0 ? CatalogTableCodec.Name(ref reader) : null;
                var column = owner is not null ? reader.U16() : 0;
                builder.Sequences.Add(new(name, increment, minimum, maximum, start, cache, last, (flags & 1) != 0, (flags & 2) != 0, owner, column));
                break;
            }
            case 3:
            {
                var flags = reader.Byte(); if ((flags & ~1) != 0) throw StorageFormat.Corrupt("invalid collation flags");
                builder.Collations.Add(new(CatalogTableCodec.Name(ref reader), reader.String(), reader.String(), reader.String(), (flags & 1) != 0)); break;
            }
            case 4: builder.Statistics.Add(ReadStatistic(ref reader)); break;
            case 5:
            {
                var name = CatalogTableCodec.Name(ref reader); var columns = new CatalogViewColumn[reader.U16()];
                if (columns.Length == 0) throw StorageFormat.Corrupt("view has no columns");
                var names = new HashSet<string>(StringComparer.Ordinal);
                for (var i = 0; i < columns.Length; i++)
                {
                    var columnName = CatalogTableCodec.Name(ref reader); if (!names.Add(columnName)) throw StorageFormat.Corrupt("duplicate view column");
                    var flags = reader.Byte(); if ((flags & ~1) != 0) throw StorageFormat.Corrupt("invalid view column flags");
                    columns[i] = new(columnName, (flags & 1) != 0 ? LongString(ref reader) : null);
                }
                var definition = LongString(ref reader); var dependencies = new CatalogViewDependency[reader.U16()];
                var seen = new HashSet<(byte, string)>();
                for (var i = 0; i < dependencies.Length; i++)
                {
                    var dependencyKind = reader.Byte(); var dependencyName = CatalogTableCodec.Name(ref reader);
                    var referenced = new string[reader.U16()];
                    if (dependencyKind > 3 || dependencyKind != 0 && referenced.Length != 0 || !seen.Add((dependencyKind, dependencyName)))
                        throw StorageFormat.Corrupt("invalid view dependency");
                    names.Clear();
                    for (var c = 0; c < referenced.Length; c++)
                    { referenced[c] = CatalogTableCodec.Name(ref reader); if (!names.Add(referenced[c])) throw StorageFormat.Corrupt("duplicate view dependency column"); }
                    dependencies[i] = new(dependencyKind, dependencyName, referenced);
                }
                var check = reader.Byte(); var viewFlags = reader.Byte();
                if (check > 2 || (viewFlags & ~3) != 0) throw StorageFormat.Corrupt("invalid view options");
                builder.Views.Add(new(name, columns, definition, dependencies, check, (viewFlags & 1) != 0, (viewFlags & 2) != 0)); break;
            }
            default: throw StorageFormat.Corrupt("unknown catalog entry kind");
        }
    }

    internal static IEnumerable<byte[]> Entries(Catalog catalog, bool encrypted = false)
    {
        foreach (var composite in (catalog.Types ?? []).OrderBy(type => type.Name, Utf8NameComparer.Instance))
        {
            var writer = new WireWriter(); writer.Byte(1); writer.String(composite.Name); CatalogTableCodec.Count(writer, composite.Fields.Length);
            foreach (var field in composite.Fields)
            {
                writer.String(field.Name); var code = StoredValues.TypeCode(field.Type.Type); writer.Byte(code);
                if (code is 14 or 15 or 17) CatalogTableCodec.WriteType(writer, field.Type, field.IntervalFields);
                writer.Byte(field.NotNull ? (byte)1 : (byte)0);
                if (code is not (14 or 15 or 17)) CatalogTableCodec.WriteType(writer, field.Type, field.IntervalFields);
            }
            yield return writer.ToArray();
        }
        foreach (var sequence in (catalog.Sequences ?? []).OrderBy(sequence => sequence.Name, Utf8NameComparer.Instance))
        {
            var writer = new WireWriter(); writer.Byte(2); writer.String(sequence.Name);
            foreach (var value in new[] { sequence.Increment, sequence.Minimum, sequence.Maximum, sequence.Start, sequence.Cache, sequence.LastValue })
                writer.U64(unchecked((ulong)value));
            writer.Byte((byte)((sequence.Cycle ? 1 : 0) | (sequence.IsCalled ? 2 : 0) | (sequence.OwnerTable is not null ? 4 : 0)));
            if (sequence.OwnerTable is not null) { writer.String(sequence.OwnerTable); CatalogTableCodec.Count(writer, sequence.OwnerColumn); }
            yield return writer.ToArray();
        }
        foreach (var collation in (catalog.Collations ?? []).OrderBy(collation => collation.Name, Utf8NameComparer.Instance))
        {
            var writer = new WireWriter(); writer.Byte(3); writer.Byte(collation.IsDefault ? (byte)1 : (byte)0);
            writer.String(collation.Name); writer.String(collation.UnicodeVersion); writer.String(collation.CldrVersion); writer.String(collation.Description);
            yield return writer.ToArray();
        }
        foreach (var table in catalog.Tables.OrderBy(table => table.Name, Utf8NameComparer.Instance)) yield return CatalogTableCodec.Write(table, encrypted);
        foreach (var statistic in (catalog.Statistics ?? []).OrderBy(item => item.Table, Utf8NameComparer.Instance).ThenBy(item => item.Column).ThenBy(item => item.Kind).ThenBy(item => item.ItemOrdinal))
            yield return WriteStatistic(statistic);
        foreach (var view in (catalog.Views ?? []).OrderBy(view => view.Name, Utf8NameComparer.Instance))
        {
            var writer = new WireWriter(); writer.Byte(5); writer.String(view.Name); CatalogTableCodec.Count(writer, view.Columns.Length);
            foreach (var column in view.Columns)
            {
                writer.String(column.Name); writer.Byte(column.DefaultExpression is null ? (byte)0 : (byte)1);
                if (column.DefaultExpression is { } expression) LongString(writer, expression);
            }
            LongString(writer, view.Definition); CatalogTableCodec.Count(writer, view.Dependencies.Length);
            foreach (var dependency in view.Dependencies.OrderBy(dep => dep.Name, Utf8NameComparer.Instance).ThenBy(dep => dep.Kind))
            {
                writer.Byte(dependency.Kind); writer.String(dependency.Name); CatalogTableCodec.Count(writer, dependency.Columns.Length);
                foreach (var column in dependency.Columns.Order(Utf8NameComparer.Instance)) writer.String(column);
            }
            writer.Byte(view.CheckOption); writer.Byte((byte)((view.SecurityBarrier ? 1 : 0) | (view.SecurityInvoker ? 2 : 0)));
            yield return writer.ToArray();
        }
    }

    private static CatalogStatistic ReadStatistic(ref WireReader reader)
    {
        var kind = reader.Byte(); var table = CatalogTableCodec.Name(ref reader); var column = reader.U16();
        if (kind > 3) throw StorageFormat.Corrupt("unknown statistic kind");
        if (kind == 0)
        {
            var flags = reader.Byte(); var analyzed = Nonnegative(ref reader); var nulls = Nonnegative(ref reader);
            var width = Nonnegative(ref reader); var distinct = Nonnegative(ref reader); var sample = reader.U32(); var nonnull = reader.U32();
            var mcv = reader.U16(); var histogram = reader.U16();
            long occurrences = 0, untracked = 0; var terms = 0;
            if ((flags & 4) != 0) { occurrences = Nonnegative(ref reader); untracked = Nonnegative(ref reader); terms = reader.U16(); }
            if ((flags & ~7) != 0 || nulls > analyzed || distinct > analyzed - nulls || (flags & 2) == 0 && (distinct != 0 || mcv != 0 || histogram != 0) ||
                sample > Math.Min(analyzed, 30000) || nonnull > sample || mcv > 100 || histogram is 1 or > 101 || terms > 100 || untracked > analyzed - nulls)
                throw StorageFormat.Corrupt("invalid statistic summary");
            return new(kind, table, column, flags, analyzed, nulls, width, distinct, sample, nonnull, mcv, histogram, occurrences, untracked, terms);
        }
        var ordinal = reader.U16(); var frequency = kind == 1 ? reader.U32() : 0;
        var rows = kind == 3 ? Nonnegative(ref reader) : 0;
        var length = reader.U16(); if (length is < 1 or > 128) throw StorageFormat.Corrupt("invalid statistic value length");
        var value = reader.Bytes(length).ToArray();
        if (kind != 3 && value[0] != 0) throw StorageFormat.Corrupt("statistic value is not nonnull inline");
        return new(kind, table, column, ItemOrdinal: ordinal, SampleFrequency: frequency, RowCount: rows, Value: value);
    }

    private static byte[] WriteStatistic(CatalogStatistic item)
    {
        var writer = new WireWriter(); writer.Byte(4); writer.Byte(item.Kind); writer.String(item.Table); CatalogTableCodec.Count(writer, item.Column);
        if (item.Kind == 0)
        {
            writer.Byte(item.Flags);
            foreach (var value in new[] { item.AnalyzedRows, item.NullCount, item.WidthSum, item.DistinctCount }) writer.U64(checked((ulong)value));
            writer.U32(item.SampleRows); writer.U32(item.SampleNonnullRows); CatalogTableCodec.Count(writer, item.McvCount); CatalogTableCodec.Count(writer, item.HistogramCount);
            if ((item.Flags & 4) != 0) { writer.U64((ulong)item.TermOccurrences); writer.U64((ulong)item.UntrackedTermRows); CatalogTableCodec.Count(writer, item.TermCount); }
        }
        else
        {
            CatalogTableCodec.Count(writer, item.ItemOrdinal);
            if (item.Kind == 1) writer.U32(item.SampleFrequency);
            if (item.Kind == 3) writer.U64((ulong)item.RowCount);
            CatalogTableCodec.Count(writer, item.Value!.Length); writer.Bytes(item.Value);
        }
        return writer.ToArray();
    }

    private static long Signed(ref WireReader reader) => unchecked((long)reader.U64());
    private static long Nonnegative(ref WireReader reader)
    {
        var value = reader.U64(); if (value > long.MaxValue) throw StorageFormat.Corrupt("negative statistic count"); return (long)value;
    }
    private static string LongString(ref WireReader reader)
    {
        var length = reader.U32(); if (length > int.MaxValue) throw StorageFormat.Corrupt("catalog string too large"); return WireReader.Utf8(reader.Bytes((int)length));
    }
    private static void LongString(WireWriter writer, string value)
    {
        var bytes = TextEncoding.Utf8.GetBytes(value); writer.U32((uint)bytes.Length); writer.Bytes(bytes);
    }
}
