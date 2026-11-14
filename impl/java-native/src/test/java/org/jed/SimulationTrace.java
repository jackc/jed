package org.jed;

import java.nio.file.*;
import java.io.IOException;
import java.util.*;

/** Strict, line-oriented shared simulation trace parser. */
record SimulationTrace(String name, long seed, int pageSize, int sector, long clock, long cacheBytes,
    boolean synchronous, long flushInterval, long workMem, long tempBuffers, long queryMemory,
    long maxStorage, boolean encrypted, boolean service, List<Step> steps) {
    record Step(int index, int line, String operation, List<String> arguments, String sql) { }
    static SimulationTrace read(Path path) throws IOException {
        Map<String,String> settings = new HashMap<>(); List<Step> steps = new ArrayList<>();
        List<String> lines = Files.readAllLines(path);
        Set<String> headers = Set.of("format", "profile", "seed", "page_size", "sector", "clock", "cache_bytes", "synchronous", "flush_interval_ms", "work_mem", "temp_buffers", "max_query_memory_bytes", "max_storage_bytes", "encrypted", "processes");
        for (int i = 0; i < lines.size(); i++) {
            String line = lines.get(i).strip(); if (line.isEmpty()) continue;
            if (line.startsWith("#")) { int colon = line.indexOf(':'); if (colon > 1) { String key = line.substring(1, colon).strip(); if (headers.contains(key) && settings.putIfAbsent(key, line.substring(colon + 1).strip()) != null) throw new IOException("duplicate simulation header " + key); } continue; }
            String[] words = line.split("\\s+", 3);
            if (words[0].equals("on")) { if (words.length != 3) throw new IOException("missing SQL at line " + (i + 1)); steps.add(new Step(steps.size(), i + 1, "on", List.of(words[1]), words[2])); continue; }
            String[] all = line.split("\\s+");
            int required = switch (all[0]) { case "open" -> 3; case "fault", "crash", "clock", "commit", "close", "rollback", "attach", "backup", "change_key", "rekey", "restore", "replay", "spawn", "exit", "kill", "disconnect" -> 2; case "reopen", "compact", "check", "corrupt" -> 1; default -> throw new IOException("unknown simulation operation at line " + (i + 1) + ": " + all[0]); };
            if (all.length < required) throw new IOException("missing arguments at line " + (i + 1));
            steps.add(new Step(steps.size(), i + 1, all[0], List.of(Arrays.copyOfRange(all, 1, all.length)), null));
        }
        if (!settings.getOrDefault("format", "").equals("simulation")) throw new IOException("missing simulation format header");
        int page = (int)number(settings, "page_size", 256), sector = (int)number(settings, "sector", 64);
        if (!org.jed.storage.PageFormat.validPageSize(page) || sector < 1 || sector > page || page % sector != 0) throw new IOException("invalid simulation page/sector size");
        String sync = settings.getOrDefault("synchronous", "on"); if (!sync.equals("on") && !sync.equals("off")) throw new IOException("invalid synchronous header");
        return new SimulationTrace(path.getFileName().toString(), Long.parseUnsignedLong(settings.getOrDefault("seed", "1")), page, sector,
            number(settings, "clock", 1700000000000000L), number(settings, "cache_bytes", 0), sync.equals("on"), number(settings, "flush_interval_ms", 0),
            number(settings, "work_mem", 0), number(settings, "temp_buffers", 0), number(settings, "max_query_memory_bytes", 0), number(settings, "max_storage_bytes", 0),
            number(settings, "encrypted", 0) == 1, settings.getOrDefault("processes", "").equals("service"), List.copyOf(steps));
    }
    private static long number(Map<String,String> settings, String name, long fallback) { return Long.parseLong(settings.getOrDefault(name, Long.toString(fallback))); }
}
