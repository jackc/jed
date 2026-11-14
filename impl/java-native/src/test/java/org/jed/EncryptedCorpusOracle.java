package org.jed;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.Map;
import java.util.HashMap;
import java.util.ArrayList;
import java.util.HexFormat;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;

/** Exact cost/plan assertions against a successful frozen independent encrypted engine run. */
final class EncryptedCorpusOracle {
    private record Entry(String relative, String hash) {}
    private record Evidence(Map<String, Entry> files, Map<String, List<String>> events) {}
    private static final Map<Path, Evidence> CACHE = new HashMap<>();
    private final String relative;
    private final List<String> events;
    private int position;
    private EncryptedCorpusOracle(String relative, List<String> events) { this.relative = relative; this.events = events; }
    static EncryptedCorpusOracle load(Path file) throws IOException {
        String directory = System.getenv("JED_JAVA_ENCRYPTED_ORACLE");
        if (directory == null) throw new IllegalStateException("Encrypted corpus needs the independent peer oracle; run mise run java-native:conformance --encrypted");
        Path root = Path.of(directory);
        String absolute = file.toAbsolutePath().normalize().toString();
        Evidence evidence = CACHE.get(root);
        if (evidence == null) {
            Map<String, Entry> files = new HashMap<>();
            for (String mapping : Files.readAllLines(root.resolve("files.tsv"), StandardCharsets.UTF_8)) {
                String[] fields = mapping.split("\t", -1);
                if (fields.length != 3) throw new IllegalStateException("Invalid encrypted oracle file mapping");
                if (files.put(fields[0], new Entry(fields[1], fields[2])) != null) throw new IllegalStateException("Duplicate encrypted oracle file mapping");
            }
            Map<String, List<String>> events = new HashMap<>();
            for (String event : Files.readAllLines(root.resolve("events.tsv"), StandardCharsets.UTF_8)) {
                if (event.isEmpty()) continue;
                int tab = event.indexOf('\t');
                if (tab < 0) throw new IllegalStateException("Invalid encrypted oracle event");
                events.computeIfAbsent(event.substring(0, tab), ignored -> new ArrayList<>()).add(event);
            }
            evidence = new Evidence(files, events); CACHE.put(root, evidence);
        }
        Entry entry = evidence.files().get(absolute);
        if (entry == null) throw new IllegalStateException("No complete independent encrypted PASS for " + file);
        try {
            String hash = HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(Files.readAllBytes(file)));
            if (!hash.equals(entry.hash())) throw new IllegalStateException("Corpus changed after independent peer capture: " + file);
        } catch (NoSuchAlgorithmException error) { throw new IllegalStateException(error); }
        return new EncryptedCorpusOracle(entry.relative(), evidence.events().getOrDefault(entry.relative(), List.of()));
    }
    private static String flat(String text) { return text.replace("\\", "\\\\").replace("\n", "\\n").replace("\t", "\\t"); }
    void check(String kind, String sql, String value) {
        String observed = relative + "\t" + kind + "\t" + flat(sql) + "\t" + flat(value);
        String expected = position < events.size() ? events.get(position) : "<no further peer event>";
        if (!expected.equals(observed)) throw new IllegalStateException("Encrypted peer event " + (position + 1) + ": expected " + expected + ", got " + observed);
        position++;
    }
    void finish() {
        if (position != events.size()) throw new IllegalStateException("Missing encrypted peer events: checked " + position + " of " + events.size());
    }
}
