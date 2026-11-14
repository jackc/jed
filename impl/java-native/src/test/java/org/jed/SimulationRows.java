package org.jed;

import java.util.*;

record SimulationRows(List<String> rendered, long affected, long cost) {
    String hash() { return fold(String.join("\u001e", rendered)); }
    static SimulationRows query(Session session, String sql) {
        List<String> result = new ArrayList<>();
        try (Rows rows = session.query(sql)) {
            for (Row row; (row = rows.next()) != null;) result.add(String.join("\u001f", row.values().stream().map(session::render).toList()));
            result.sort(SqlText::compare); return new SimulationRows(result, rows.affectedRows(), rows.cost());
        }
    }
    static String digest(Session session, SimulationFileSystem fs) {
        return digest(session, fs, "main");
    }
    static String digest(Session session, SimulationFileSystem fs, String domain) {
        String prefix = domain.equals("main") ? "" : domain + ".";
        boolean was = fs.oracleReads; fs.oracleReads = true; long memory = session.maxQueryMemoryBytes(); session.setMaxQueryMemoryBytes(0);
        try {
            StringBuilder text = new StringBuilder(); List<String> tables = List.of();
            for (String relation : List.of("jed_tables", "jed_columns", "jed_indexes", "jed_constraints", "jed_sequences")) {
                String sql = "SELECT * FROM " + prefix + relation; var rows = query(session, sql);
                if (relation.equals("jed_tables")) tables = rows.rendered.stream().map(row -> row.split("\u001f", -1)[0]).sorted(SqlText::compare).toList();
                text.append(sql).append('\n').append(String.join("\n", rows.rendered)).append("\n\n");
            }
            for (String table : tables) { String sql = "SELECT * FROM " + prefix + "\"" + table.replace("\"", "\"\"") + "\""; var rows = query(session, sql); text.append(sql).append('\n').append(String.join("\n", rows.rendered)).append("\n\n"); }
            return fold(text.toString());
        } finally { fs.oracleReads = was; session.setMaxQueryMemoryBytes(memory); }
    }
    static String fold(String text) { return fold(SqlText.encode(text)); }
    static String fold(byte[] bytes) { long hash = 0xcbf29ce484222325L; for (byte b : bytes) hash = (hash ^ (b & 255)) * 0x100000001b3L; return String.format(Locale.ROOT, "%016x", hash); }
}
