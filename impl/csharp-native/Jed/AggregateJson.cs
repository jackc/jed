namespace Jed;

internal sealed class AggregateJson
{
    private bool seen;
    private readonly List<SqlJsonNode> elements = [];
    private readonly List<SqlJsonMember> members = [];
    private readonly List<string> rendered = [];
    private readonly HashSet<string> keys = new(StringComparer.Ordinal);
    internal static bool IsJson(string name) => name is "json_agg" or "jsonb_agg" or "json_agg_strict" or "jsonb_agg_strict" or
        "json_object_agg" or "jsonb_object_agg" or "json_object_agg_unique" or "jsonb_object_agg_unique" or
        "json_object_agg_strict" or "jsonb_object_agg_strict" or "json_object_agg_unique_strict" or "jsonb_object_agg_unique_strict";
    internal static bool Object(string name) => name.Contains("object_agg", StringComparison.Ordinal);
    internal void Add(AggregatePlan plan, SqlValue value, SqlValue[] extra, Meter meter)
    {
        seen = true;
        var binary = plan.Type == SqlType.Jsonb;
        var objectAggregate = Object(plan.Name);
        var image = objectAggregate ? extra[0] : value;
        var strict = plan.Name.EndsWith("_strict", StringComparison.Ordinal);
        if (!objectAggregate && strict && image.IsNull) return;
        meter.Charge(Costs.GeneratedRow + (TemporalRender.ZoneCount(value) + (objectAggregate ? TemporalRender.ZoneCount(image) : 0)) * Costs.Timezone); meter.Guard();
        if (objectAggregate)
        {
            var key = JsonImage.Key(value, meter.Temporal, binary);
            if (plan.Name.Contains("_unique", StringComparison.Ordinal) && !keys.Add(key)) throw new SqlException("22030", "duplicate JSON object key value");
            if (strict && image.IsNull) return;
            if (binary) members.Add(new(key, JsonImage.Node(image, meter.Temporal)));
            else rendered.Add(JsonImage.Quote(key) + " : " + JsonImage.Text(image, meter.Temporal));
        }
        else if (binary) elements.Add(JsonImage.Node(image, meter.Temporal));
        else rendered.Add((rendered.Count != 0 && !image.IsNull && image.Type.Kind is SqlKind.Array or SqlKind.Composite ? "\n " : "") + JsonImage.Text(image, meter.Temporal));
    }
    internal SqlValue Result(AggregatePlan plan)
    {
        if (!seen) return SqlValue.Null(plan.Type);
        if (plan.Type == SqlType.Jsonb) return SqlValue.Jsonb(Object(plan.Name) ? SqlJsonNode.Object(members) : SqlJsonNode.Array(elements));
        var text = Object(plan.Name) ? "{ " + string.Join(", ", rendered) + " }" :
            "[" + string.Join(", ", rendered) + "]";
        return SqlValue.Json(text);
    }
}
