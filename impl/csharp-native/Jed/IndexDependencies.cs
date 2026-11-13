namespace Jed;

internal static partial class StatementExecution
{
    private static void ValidateTemporalDependencies(Resolved expression)
    {
        if (expression.SessionDependent) throw new SqlException(SqlStates.InvalidObjectDefinition, "index literal reads the session time zone");
        if (expression.Op == "cast" && expression.Args.Length == 1)
        {
            var source = expression.Args[0].Type; var target = expression.Type;
            static bool ContainsZoned(SqlType type) => type == SqlType.Timestamptz || type.Element is { } element && ContainsZoned(element) || type.Fields.Any(field => ContainsZoned(field.Type));
            if (source == SqlType.Text && ContainsZoned(target))
                throw new SqlException(SqlStates.InvalidObjectDefinition, "index input reads the session time zone");
            if (source == SqlType.Timestamptz && target.Kind is SqlKind.Date or SqlKind.Timestamp || target == SqlType.Timestamptz && source.Kind is SqlKind.Date or SqlKind.Timestamp)
                throw new SqlException(SqlStates.InvalidObjectDefinition, "index cast reads the session time zone");
        }
        if (expression.Op == "extract" && expression.Args[0].Type == SqlType.Timestamptz && expression.Text != "epoch")
            throw new SqlException(SqlStates.InvalidObjectDefinition, "index extraction reads the session time zone");
        if (expression.Op != "call") return;
        var name = expression.Text; var args = expression.Args;
        if (name == "date_part" && args[1].Type == SqlType.Timestamptz &&
            (args[0].Op != "constant" || args[0].Constant.IsNull || TextEncoding.FoldName(args[0].Constant.AsText()) != "epoch"))
            throw new SqlException(SqlStates.InvalidObjectDefinition, "index extraction reads the session time zone");
        if (name == "date_trunc" && args[1].Type == SqlType.Timestamptz && args.Length == 2 || name == "make_timestamptz" && args.Length == 6)
            throw new SqlException(SqlStates.InvalidObjectDefinition, "index function reads the session time zone");
        // These overloads have argument-sensitive admission stricter or narrower
        // than the function catalog's general volatility label.
        var specialized = name is "date_part" or "date_trunc" or "make_timestamptz" or "age" or "to_char" or "to_json" or "to_jsonb" or "array_to_json" or "row_to_json" or "json_build_array" or "jsonb_build_array" or "json_build_object" or "jsonb_build_object";
        if (!specialized && expression.Function is { Volatility: not "immutable" })
            throw new SqlException(SqlStates.InvalidObjectDefinition, "index function is not immutable");
    }

    private static CatalogIndex PinIndexDependencies(CatalogIndex index, IEnumerable<Resolved> expressions, Meter meter)
    {
        var pins = new Dictionary<string, CatalogTimezoneDependency>(StringComparer.Ordinal); var dynamic = false;
        void Pin(string name, bool loaded = false)
        {
            var rule = loaded ? meter.Temporal.Data!.LoadedRule(name) : meter.Temporal.ResolveZone(name);
            if (rule.IsFixed) return;
            if (rule.Version.Length == 0) throw new SqlException(SqlStates.InvalidObjectDefinition, "loaded time-zone data has no version");
            var data = meter.Temporal.Data!;
            var loadedName = data.ZoneNames.Concat(data.Links.Keys).FirstOrDefault(candidate => candidate == name) ??
                data.ZoneNames.Concat(data.Links.Keys).OrderBy(candidate => candidate, Utf8NameComparer.Instance).First(candidate => TextEncoding.FoldName(candidate) == TextEncoding.FoldName(name));
            if (TextEncoding.ByteCount(loadedName) > ushort.MaxValue || TextEncoding.ByteCount(rule.Version) > ushort.MaxValue)
                throw new SqlException(SqlStates.ProgramLimitExceeded, "time-zone dependency exceeds catalog bounds");
            pins[loadedName] = new(loadedName, rule.Version, rule.Checksum);
        }
        void Visit(Resolved expression)
        {
            Resolved? zone = expression.Op == "at_time_zone" ? expression.Args[1] : expression.Op == "call" && expression.Text == "date_trunc" && expression.Args.Length == 3 ? expression.Args[2] :
                expression.Op == "call" && expression.Text == "make_timestamptz" && expression.Args.Length == 7 ? expression.Args[6] : null;
            if (zone is not null)
            {
                if (zone.Op == "constant") { if (!zone.Constant.IsNull) Pin(zone.Constant.AsText()); }
                else
                {
                    dynamic = true;
                    if (meter.Temporal.Data is { } data) foreach (var name in data.ZoneNames.Concat(data.Links.Keys)) Pin(name, loaded: true);
                }
            }
            foreach (var argument in expression.Args) Visit(argument);
        }
        foreach (var expression in expressions) { ValidateIndexDependencies(expression); Visit(expression); }
        if (pins.Count > ushort.MaxValue) throw new SqlException(SqlStates.ProgramLimitExceeded, "too many time-zone dependencies");
        return index with { TimezoneDependencies = pins.Count != 0 || dynamic ? pins.Values.OrderBy(pin => pin.Name, Utf8NameComparer.Instance).ToArray() : null, DynamicTimezone = dynamic };
    }

    internal static bool IndexDependenciesUsable(CatalogIndex index, Catalog catalog)
    {
        if (index.TimezoneDependencies is not { } pins) return true;
        var data = catalog.BoundTimeZones;
        if (index.DynamicTimezone)
        {
            var names = data is null ? Array.Empty<string>() : data.ZoneNames.Concat(data.Links.Keys).OrderBy(name => name, Utf8NameComparer.Instance).ToArray();
            if (!names.SequenceEqual(pins.Select(pin => pin.Name), StringComparer.Ordinal)) return false;
        }
        foreach (var pin in pins)
        {
            if (data is null) return false;
            try
            {
                var rule = data.LoadedRule(pin.Name);
                if (rule.Version != pin.Version || rule.Checksum != pin.Checksum) return false;
            }
            catch (SqlException) { return false; }
        }
        return true;
    }
    private static void WritableIndexDependencies(CatalogTable table, Catalog catalog)
    {
        if ((table.Indexes ?? []).Any(index => !IndexDependenciesUsable(index, catalog)))
            throw new SqlException(SqlStates.CollationVersionMismatch, "index dependency differs from the loaded environment; rebuild the index");
    }
}
