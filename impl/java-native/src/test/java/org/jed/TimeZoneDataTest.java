package org.jed;

import java.nio.charset.StandardCharsets;
import java.util.Arrays;

/** Canonical byte-level timezone reader vectors and immutable host environment boundaries. */
public final class TimeZoneDataTest {
    public static void main(String[] args) throws Exception {
        byte[] bytes;
        try (var input = TimeZoneDataTest.class.getResourceAsStream("/tzdata.jtz")) { bytes = input.readAllBytes(); }
        TimeZoneData data = TimeZoneData.load(bytes);
        if (!data.version().equals("2026a") || !Arrays.equals(bytes, data.save())) throw new AssertionError("bundle round trip");
        Environment environment = Environment.builder().loadTimeZoneData(bytes).build();
        String fixtures;
        try (var input = TimeZoneDataTest.class.getResourceAsStream("/timezones.tsv")) { fixtures = new String(input.readAllBytes(), StandardCharsets.UTF_8); }
        int count = 0;
        for (String line : fixtures.split("\n")) {
            if (line.startsWith("#") || line.isEmpty()) continue;
            String[] fields = line.split("\t");
            TimeZoneData.Offset actual = environment.zone(fields[0]).offsetAt(Long.parseLong(fields[1]));
            if (actual.seconds() != Integer.parseInt(fields[2]) || !actual.abbreviation().equals(fields[3]) || actual.daylight() != Boolean.parseBoolean(fields[4])) throw new AssertionError(line + " got " + actual);
            count++;
        }
        byte[] mutated = bytes.clone(); mutated[20] ^= 1;
        try { TimeZoneData.load(mutated); throw new AssertionError("bad CRC accepted"); } catch (SqlException expected) { if (!expected.sqlState().equals("XX001")) throw expected; }
        try { Environment.empty().zone("America/New_York"); throw new AssertionError("ambient timezone data"); } catch (SqlException expected) { if (!expected.sqlState().equals("22023")) throw expected; }
        TimeZoneData.Zone zone = environment.zone("america/new_york");
        long gap = Temporal.parseTimestamp("2024-03-10 02:30:00", false);
        long overlap = Temporal.parseTimestamp("2024-11-03 01:30:00", false);
        if (zone.toInstant(gap) != Temporal.parseTimestamp("2024-03-10 07:30:00", false) || zone.toInstant(overlap) != Temporal.parseTimestamp("2024-11-03 06:30:00", false)) throw new AssertionError("DST resolution");
        System.out.println("TimeZoneDataTest: " + count + " shared reader vectors, bundle roundtrip and environment isolation passed");
    }
}
