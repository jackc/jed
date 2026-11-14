package org.jed;

/** Unrecognized persisted SQL remains intact until an operation needs its parsed form. */
final class StoredDefinitions {
    private StoredDefinitions() {}
    static SqlException error(String detail) {
        return new SqlException("XX003", "stored definition does not parse in this version of jed: " + detail);
    }
    static boolean parsed(SqlParser.Expr expression) {
        return !(DefaultExpression.unwrap(expression) instanceof SqlParser.UnparsedExpression);
    }
    static void require(SqlParser.Expr expression, String detail) {
        if (!parsed(expression)) throw error(detail);
    }
    static boolean parsed(Engine.Index index) {
        if (!parsed(index.predicate())) return false;
        for (Engine.IndexKey key : index.keys()) if (!parsed(key.expression())) return false;
        return true;
    }
    static void require(Engine.Index index) {
        for (Engine.IndexKey key : index.keys()) if (DefaultExpression.unwrap(key.expression()) instanceof SqlParser.UnparsedExpression unparsed) throw error(unparsed.detail());
        if (DefaultExpression.unwrap(index.predicate()) instanceof SqlParser.UnparsedExpression unparsed) throw error(unparsed.detail());
    }
    static void alter(Engine.Table table, SqlParser.AlterAction action) {
        boolean rename = action.action().equals("rename_table") || action.action().equals("rename_column");
        boolean drop = action.action().equals("drop_column") || action.action().equals("drop_column_cascade");
        boolean type = action.action().equals("type");
        if (!rename && !drop && !type) return;
        for (Engine.Check check : table.checks()) require(check.expression(), "check constraint \"" + check.name() + "\" of table \"" + table.name() + "\"");
        for (Engine.Column column : table.columns()) if (rename || type && column.name().equals(action.name()))
            require(column.defaultValue(), "default of column \"" + column.name() + "\" of table \"" + table.name() + "\"");
        for (Engine.Index index : table.indexes()) require(index);
    }
}
