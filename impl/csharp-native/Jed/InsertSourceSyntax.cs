namespace Jed;

internal sealed partial class Parser
{
    private static void RejectDefaultSource(SelectSyntax query)
    {
        void Expression(Expression expression)
        {
            if (expression.Op == "default") throw Errors.Syntax("DEFAULT is only valid in the INSERT's own VALUES list");
            foreach (var argument in expression.Args) Expression(argument);
            if (expression.Query is { } nested) RejectDefaultSource(nested);
            if (expression.Predicate is { } predicate) Expression(predicate);
        }
        void Relation(RelationSyntax? source)
        {
            switch (source)
            {
                case ValuesRelation values: foreach (var row in values.Values) foreach (var value in row) Expression(value); break;
                case SetRelation set: RejectDefaultSource(set.Left); RejectDefaultSource(set.Right); break;
                case DerivedRelation derived: RejectDefaultSource(derived.Query); break;
                case JoinedRelation join: Relation(join.Left); Relation(join.Right); if (join.On is { } on) Expression(on); break;
                case GroupedRelation group: Relation(group.Source); break;
                case AliasedRelation alias: Relation(alias.Source); break;
                case LateralRelation lateral: Relation(lateral.Source); break;
                case FunctionRelation function: Expression(function.Function); break;
            }
        }
        Relation(query.Relation);
        foreach (var item in query.Items) Expression(item.Expression);
        foreach (var cte in query.With ?? []) RejectDefaultSource(cte.Query);
    }
}
