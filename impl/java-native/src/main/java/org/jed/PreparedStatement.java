package org.jed;

/** Parsed immutable SQL, independent of the handle that prepared it and safe to share across sessions. */
public final class PreparedStatement {
    private final SqlParser.Statement syntax;
    private final int parameters;
    PreparedStatement(String sql) {
        syntax = new SqlParser(sql).parseStatement();
        int count = 0;
        // Stored definitions reject placeholders themselves with 42P02. Their tokens
        // do not create runtime bind slots on the surrounding DDL statement.
        boolean accepts = syntax instanceof SqlParser.Select || syntax instanceof SqlParser.Values || syntax instanceof SqlParser.SetOperation || syntax instanceof SqlParser.QueryTail
            || syntax instanceof SqlParser.With || syntax instanceof SqlParser.Insert || syntax instanceof SqlParser.Update || syntax instanceof SqlParser.Delete || syntax instanceof SqlParser.Merge || syntax instanceof SqlParser.Explain
            || syntax instanceof SqlParser.CreateTable create && create.asQuery() != null;
        if (accepts) for (Lexer.Token token : new Lexer(sql).scan()) if (token.kind() == Lexer.Kind.PARAMETER) {
            try { count = Math.max(count, Integer.parseInt(token.text())); }
            catch (NumberFormatException failure) { throw new SqlException("42601", "parameter index out of range"); }
        }
        parameters = count;
    }
    int parameterCount() { return parameters; }
    SqlParser.Statement syntax(SqlValue[] arguments) {
        if (parameters == 0 && arguments.length != 0) throw new SqlException("42601", "parameter count mismatch");
        for (SqlValue argument : arguments) if (argument == null) throw new SqlException("22004", "null Java parameter; use a typed SQL NULL value");
        return syntax;
    }
}
