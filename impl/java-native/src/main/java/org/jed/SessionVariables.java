package org.jed;

/** Session settings are supplied through the host seam and never read from the OS. */
final class SessionVariables {
    private SessionVariables() {}
    static String name(String input) {
        SqlText.validate(input); StringBuilder result = new StringBuilder(input.length());
        for (int i = 0; i < input.length(); i++) { char c = input.charAt(i); result.append(c >= 'A' && c <= 'Z' ? (char)(c + 32) : c); }
        return result.toString();
    }
    static ScalarPlan.Expression resolve(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        if (!call.name().equals("current_setting")) return null;
        if (call.args().isEmpty() || call.args().size() > 2) throw new SqlException("42883", "current_setting argument count is invalid");
        ScalarPlan.Expression key = resolver.resolve(call.args().getFirst(), SqlType.TEXT);
        ScalarPlan.Expression missing = call.args().size() == 1 ? ScalarPlan.constant(SqlValue.ofBoolean(false)) : resolver.resolve(call.args().get(1), SqlType.BOOLEAN);
        if (key.type().kind() != SqlType.Kind.TEXT || missing.type() != SqlType.BOOLEAN) throw new SqlException("42883", "current_setting requires text and boolean arguments");
        return new ScalarPlan.Expression(SqlType.TEXT, meter -> {
            meter.operation(); SqlValue name = key.run(meter); if (name.isNull()) return SqlValue.nullValue(SqlType.TEXT);
            SqlValue allowed = missing.run(meter); if (allowed.isNull()) return SqlValue.nullValue(SqlType.TEXT);
            String value = meter.variable(name.asText());
            if (value == null && !allowed.asBoolean()) throw new SqlException("42704", "unrecognized configuration parameter: " + name.asText());
            return value == null ? SqlValue.nullValue(SqlType.TEXT) : SqlValue.ofText(value);
        });
    }
}
