package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Expand composite stars into ordinary field selections before resolving output clauses. */
final class ProjectionSyntax {
    private ProjectionSyntax() {}
    static List<SqlParser.Item> expand(List<SqlParser.Item> items, ScalarPlan.Resolver resolver) {
        List<SqlParser.Item> output = new ArrayList<>();
        for (SqlParser.Item item : items) {
            if (item.expression() instanceof SqlParser.Field field && field.name() == null) {
                SqlType type = resolver.resolve(field.base(), null).type();
                if (type.kind() != SqlType.Kind.COMPOSITE && type.kind() != SqlType.Kind.RECORD) throw new SqlException("42809", "star expansion requires composite value");
                for (SqlType.Field member : type.fields()) output.add(new SqlParser.Item(new SqlParser.Field(field.base(), member.name()), null));
            } else output.add(item);
        }
        return output;
    }
}
