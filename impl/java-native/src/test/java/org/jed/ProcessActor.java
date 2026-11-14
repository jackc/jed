package org.jed;

import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HexFormat;
import java.util.Map;

/** Test-only line actor for the frozen real-process corpus. */
public final class ProcessActor {
    private ProcessActor() { }
    public static void main(String[] args) {
        if (args.length >= 2 && (args[0].equals("pages") || args[0].equals("pages-read-only"))) {
            try { PageRelay.serve(Path.of(args[1]), System.in, System.out, args[0].equals("pages-read-only")); }
            catch (Exception failure) { System.err.println("page relay failed: " + failure.getMessage()); System.exit(1); }
            return;
        }
        try { run(args); }
        catch (SqlException failure) { error(failure); System.exit(1); }
        catch (Exception failure) { error(new SqlException("58030", failure.toString())); System.exit(1); }
    }
    private static void run(String[] args) throws Exception {
        if (args.length < 2) throw new IllegalArgumentException("process-actor create|open PATH [options]");
        Map<String,String> options = new HashMap<>(); var positional = new ArrayList<String>();
        for (int i = 2; i < args.length; i++) { int equals = args[i].indexOf('='); if (equals < 0) positional.add(args[i]); else options.put(args[i].substring(0, equals), args[i].substring(equals + 1)); }
        FileOptions.Builder builder = FileOptions.builder();
        if (!positional.isEmpty()) builder.lockTimeoutMillis(Long.parseLong(positional.getFirst()));
        if (positional.size() > 1) builder.idleTransactionMillis(Long.parseLong(positional.get(1)));
        for (var entry : options.entrySet()) switch (entry.getKey()) {
            case "key" -> builder.credential(key(entry.getValue()));
            case "ownership" -> builder.ownership(FileOptions.Ownership.valueOf(entry.getValue().toUpperCase(java.util.Locale.ROOT)));
            case "yield_grace" -> builder.yieldGraceMillis(Long.parseLong(entry.getValue()));
            case "shutdown_grace" -> builder.shutdownGraceMillis(Long.parseLong(entry.getValue()));
            case "idle_session" -> builder.idleSessionMillis(Long.parseLong(entry.getValue()));
            case "max_connections" -> builder.maxConnections(Integer.parseInt(entry.getValue()));
            case "page_size" -> builder.pageSize(Integer.parseInt(entry.getValue()));
            case "synchronous" -> builder.synchronous(entry.getValue().equals("on"));
            default -> throw new SqlException("0A000", "unsupported process option: " + entry.getKey());
        }
        String action = args[0]; Path path = Path.of(args[1]);
        if (action.equals("open-read-only") || action.equals("open-remote-read-only")) builder.readOnly(true);
        if (action.equals("open-exclusive")) builder.serve(false);
        if (!java.util.Set.of("create", "open", "open-read-only", "open-exclusive", "open-remote", "open-remote-read-only").contains(action))
            throw new SqlException("0A000", "unsupported process transport: " + action);
        Database database;
        if (action.startsWith("open-remote")) {
            String server = System.getenv("JED_PAGES_ACTOR");
            if (server == null || server.isEmpty()) throw new IllegalArgumentException("open-remote requires JED_PAGES_ACTOR");
            String serving = action.endsWith("read-only") ? "pages-read-only" : "pages";
            database = Database.openRemote(path.toString(), () -> {
                Process process = new ProcessBuilder(server, serving, path.toString()).redirectError(ProcessBuilder.Redirect.INHERIT).start();
                return new PageTransport.Connection(process.getInputStream(), process.getOutputStream(), () -> {
                    process.destroy(); process.getInputStream().close(); process.getOutputStream().close();
                });
            }, builder.build());
        } else database = action.equals("create") ? Database.create(path, builder.build()) : Database.open(path, builder.build());
        Session reader = null, writer = null;
        try (database; BufferedReader input = new BufferedReader(new InputStreamReader(System.in, StandardCharsets.UTF_8))) {
            System.out.println("READY"); System.out.flush();
            String line;
            while ((line = input.readLine()) != null) {
                int tab = line.indexOf('\t'); String command = tab < 0 ? line : line.substring(0, tab), argument = tab < 0 ? "" : line.substring(tab + 1);
                try {
                    String value = "";
                    switch (command) {
                        case "EXEC" -> { try (Session session = database.session()) { execute(session, sql(argument)); } }
                        case "QUERY_I64" -> { try (Session session = database.session()) { value = integer(session, sql(argument)); } }
                        case "ATTACH" -> { String[] fields = argument.split("\t", 3); database.attach(fields[0], AttachSource.file(Path.of(fields[2])), fields[1].equals("1")); }
                        case "DETACH" -> database.detach(argument);
                        case "READ_OPEN" -> { if (reader != null) reader.close(); reader = database.session(); execute(reader, "BEGIN READ ONLY"); }
                        case "READ_QUERY_I64" -> value = integer(reader, sql(argument));
                        case "READ_CLOSE" -> { if (reader != null) reader.close(); reader = null; }
                        case "WRITE_OPEN" -> { if (writer != null) writer.close(); writer = database.session(); writer.setLockTimeoutMillis(argument.isEmpty() ? 0 : Long.parseLong(argument)); execute(writer, "BEGIN"); }
                        case "WRITE_EXEC" -> execute(writer, sql(argument));
                        case "WRITE_COMMIT" -> execute(writer, "COMMIT");
                        case "WRITE_ROLLBACK" -> execute(writer, "ROLLBACK");
                        case "BACKUP" -> { String[] fields = argument.split("\\t", 2); database.backupForTesting(Path.of(fields[1]), fields[0].equals("page_identical")); }
                        case "COMPACT" -> database.compact(argument.isEmpty() ? "main" : argument);
                        case "REKEY" -> database.rekey(argument.isEmpty() ? null : key(argument));
                        case "CHANGE_KEY" -> database.changeCredential(key(argument));
                        case "MAX_STORAGE_BYTES" -> database.setMaxStorageBytes(Long.parseLong(argument));
                        case "STORAGE_BYTES" -> value = Long.toString(database.storageBytes());
                        case "ROLE" -> value = database.processRole(argument);
                        case "TXID" -> value = Long.toUnsignedString(database.processTxid());
                        case "PAGE_COUNT" -> value = Long.toUnsignedString(database.processPageCount());
                        case "HANGUP_NEXT_ACK" -> database.interruptNextAcknowledgement(false);
                        case "CRASH_NEXT_ACK" -> database.interruptNextAcknowledgement(true);
                        case "CLOSE" -> { if (reader != null) reader.close(); reader = null; if (writer != null) writer.close(); writer = null; ok(""); return; }
                        default -> throw new SqlException("0A000", "unsupported process command: " + command);
                    }
                    ok(value);
                } catch (SqlException failure) { error(failure); }
            }
        } finally { if (reader != null) reader.close(); if (writer != null) writer.close(); }
    }
    private static Credential key(String text) { return Credential.hostKey(HexFormat.of().parseHex(text)); }
    private static String sql(String text) { return new String(HexFormat.of().parseHex(text), StandardCharsets.UTF_8); }
    private static void execute(Session session, String sql) { try (Rows rows = session.query(sql)) { while (rows.next() != null) { } } }
    private static String integer(Session session, String sql) { try (Rows rows = session.query(sql)) {
        var output = new ArrayList<String>(); Row row; while ((row = rows.next()) != null) { var cells = new ArrayList<String>(); for (SqlValue cell : row.values()) cells.add(cell.isNull() ? "NULL" : Long.toString(cell.asInteger())); output.add(String.join(":", cells)); }
        return String.join(",", output);
    } }
    private static void ok(String value) { System.out.println("OK\t" + value); System.out.flush(); }
    private static void error(SqlException failure) { System.out.println("ERR\t" + failure.sqlState() + "\t" + HexFormat.of().formatHex(java.util.Objects.toString(failure.getMessage(), failure.sqlState()).getBytes(StandardCharsets.UTF_8))); System.out.flush(); }
}
