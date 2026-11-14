package org.jed;

import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeSet;
import java.util.zip.CRC32;
import org.jed.storage.Lz4;

/** Immutable host-supplied JTZ bundle and independent RFC 8536 reader. */
public final class TimeZoneData {
    public record Offset(int seconds, String abbreviation, boolean daylight) {}
    private final byte[] bytes;
    private final String version;
    private final Map<String, Zone> zones;
    private final Map<String, Integer> hashes;
    private TimeZoneData(byte[] bytes, String version, Map<String, Zone> zones, Map<String, Integer> hashes) {
        this.hashes = Map.copyOf(hashes); this.bytes = bytes; this.version = version; this.zones = java.util.Collections.unmodifiableMap(zones);
    }
    public String version() { return version; }
    public byte[] save() { return bytes.clone(); }
    public List<String> names() { return List.copyOf(zones.keySet()); }
    Zone named(String name) {
        Zone exact = zones.get(name);
        if (exact != null) return exact;
        String folded = fold(name);
        for (Map.Entry<String, Zone> item : zones.entrySet()) if (fold(item.getKey()).equals(folded)) return item.getValue();
        return null;
    }
    IndexDependencies.ZonePin pin(String name) {
        String exact = zones.containsKey(name) ? name : null;
        if (exact == null) for (String candidate : zones.keySet()) if (fold(candidate).equals(fold(name))) { exact = candidate; break; }
        return exact == null ? null : new IndexDependencies.ZonePin(exact, version, hashes.get(exact));
    }
    public static TimeZoneData load(byte[] source) {
        byte[] bytes = source.clone();
        try {
            if (bytes.length < 16 || crc(bytes, 0, bytes.length - 4) != ByteBuffer.wrap(bytes, bytes.length - 4, 4).getInt()) throw invalid();
            Reader in = new Reader(bytes);
            if (!Arrays.equals(in.bytes(6), new byte[]{'J', 'T', 'Z', 0, 0, 0}) || in.u16() != 1) throw invalid();
            String version = in.string(); in.string();
            int count = in.u16(), links = in.u16();
            record Entry(String name, int hash, int raw, int length, int position) {}
            List<Entry> entries = new ArrayList<>(); String previous = null;
            for (int i = 0; i < count; i++) {
                String name = in.string();
                if (previous != null && SqlText.compare(previous, name) >= 0) throw invalid();
                previous = name;
                int hash = in.i32(), raw = in.i32();
                if (raw < 0) throw invalid();
                entries.add(new Entry(name, hash, raw, in.count(), in.count()));
            }
            Map<String, String> aliases = new LinkedHashMap<>(); previous = null;
            for (int i = 0; i < links; i++) {
                String name = in.string(), target = in.string();
                if (previous != null && SqlText.compare(previous, name) >= 0) throw invalid();
                previous = name; aliases.put(name, target);
            }
            int body = in.position;
            Map<String, Zone> zones = new LinkedHashMap<>();
            Map<String, Integer> hashes = new LinkedHashMap<>();
            for (Entry entry : entries) {
                if (entry.position != body || entry.length > bytes.length - 4 - body || entry.raw > 64 * 1024 * 1024) throw invalid();
                byte[] raw = Lz4.decompress(Arrays.copyOfRange(bytes, body, body + entry.length), entry.raw, 64 * 1024 * 1024);
                if (crc(raw, 0, raw.length) != entry.hash) throw invalid();
                hashes.put(entry.name, entry.hash); zones.put(entry.name, parseZone(entry.name, raw)); body += entry.length;
            }
            if (body != bytes.length - 4) throw invalid();
            for (Map.Entry<String, String> alias : aliases.entrySet()) {
                Zone target = zones.get(alias.getValue());
                if (target == null || zones.containsKey(alias.getKey())) throw invalid();
                zones.put(alias.getKey(), target); hashes.put(alias.getKey(), hashes.get(alias.getValue()));
            }
            return new TimeZoneData(bytes, version, zones, hashes);
        } catch (java.io.IOException | IndexOutOfBoundsException | ArithmeticException failure) { throw invalid(); }
    }
    private static int crc(byte[] bytes, int start, int length) { CRC32 crc = new CRC32(); crc.update(bytes, start, length); return (int) crc.getValue(); }
    static String fold(String text) {
        StringBuilder result = new StringBuilder(text.length());
        for (int i = 0; i < text.length(); i++) { char c = text.charAt(i); result.append(c >= 'A' && c <= 'Z' ? (char) (c + 32) : c); }
        return result.toString();
    }
    static Zone builtin(String name, boolean iso) {
        if (fold(name).equals("utc")) return Zone.fixed("UTC", 0);
        if (name.isEmpty() || name.charAt(0) != '+' && name.charAt(0) != '-') return null;
        try {
            if (iso && name.indexOf(':') < 0 && (name.length() == 4 || name.length() == 5)) {
                String expanded = name.substring(0, name.length() - 2) + ":" + name.substring(name.length() - 2);
                return builtin(expanded, true);
            }
            Posix parser = new Posix(name); int seconds = parser.offset(iso ? 15 : 167, iso ? 59 : 60);
            if (!parser.end()) throw invalid();
            return Zone.fixed(name, iso ? seconds : -seconds);
        } catch (SqlException failure) { throw new SqlException("22023", "time zone not recognized: " + name); }
    }
    public static final class Zone {
        private final String name;
        private final long[] transitions;
        private final int[] indices;
        private final Offset[] types;
        private final Offset initial;
        private final Footer footer;
        private final int[] offsets;
        private Zone(String name, long[] transitions, int[] indices, Offset[] types, Footer footer) {
            this.name = name; this.transitions = transitions; this.indices = indices; this.types = types; this.footer = footer;
            Offset initial = types[0];
            for (Offset type : types) if (!type.daylight) { initial = type; break; }
            this.initial = initial;
            TreeSet<Integer> offsets = new TreeSet<>();
            for (Offset type : types) offsets.add(type.seconds);
            if (footer != null) { offsets.add(footer.standard.seconds); if (footer.daylight != null) offsets.add(footer.daylight.seconds); }
            this.offsets = offsets.stream().mapToInt(Integer::intValue).toArray();
        }
        static Zone fixed(String name, int seconds) {
            int magnitude = Math.abs(seconds);
            String abbreviation = seconds == 0 ? "UTC" : String.format(java.util.Locale.ROOT, "%s%02d:%02d", seconds < 0 ? "-" : "+", magnitude / 3600, magnitude / 60 % 60)
                + (magnitude % 60 == 0 ? "" : String.format(java.util.Locale.ROOT, ":%02d", magnitude % 60));
            return new Zone(name, new long[0], new int[0], new Offset[]{new Offset(seconds, abbreviation, false)}, null);
        }
        public String name() { return name; }
        public Offset offsetAt(long micros) { return seconds(Math.floorDiv(micros, 1_000_000)); }
        private Offset seconds(long instant) {
            if (footer != null && (transitions.length == 0 || instant >= transitions[transitions.length - 1])) return footer.at(instant);
            if (transitions.length == 0 || instant < transitions[0]) return initial;
            int low = 0, high = transitions.length;
            while (low < high) { int middle = (low + high) >>> 1; if (transitions[middle] <= instant) low = middle + 1; else high = middle; }
            return types[indices[low - 1]];
        }
        public long toLocal(long instant) { return TemporalOps.infinite(instant) ? instant : shift(instant, offsetAt(instant).seconds); }
        public long toInstant(long local) {
            if (TemporalOps.infinite(local)) return local;
            Long valid = null, gap = null; long gapDistance = Long.MAX_VALUE;
            for (int offset : offsets) {
                long candidate;
                try { candidate = shift(local, -offset); }
                catch (SqlException failure) { continue; }
                int actual = offsetAt(candidate).seconds;
                long difference = (long) actual - offset;
                if (difference == 0 && (valid == null || candidate > valid)) valid = candidate;
                else if (difference > 0 && difference < gapDistance) {
                    // A gap is a two-way mismatch between the offsets on its two sides.
                    // Historic unused offsets must not compete with the current transition.
                    try {
                        if (offsetAt(shift(local, -actual)).seconds == offset) { gap = candidate; gapDistance = difference; }
                    } catch (SqlException failure) { /* This alternative cannot represent a finite instant. */ }
                }
            }
            if (valid != null) return valid;
            if (gap != null) return gap;
            throw Temporal.overflow();
        }
        private static long shift(long instant, int seconds) {
            try { long result = Math.addExact(instant, seconds * 1_000_000L); if (TemporalOps.infinite(result)) throw Temporal.overflow(); return result; }
            catch (ArithmeticException failure) { throw Temporal.overflow(); }
        }
    }
    private static Zone parseZone(String name, byte[] bytes) {
        Reader in = new Reader(bytes); Header header = header(in); int width = 4;
        if (header.version != 0) { in.skip(header.size(4)); header = header(in); width = 8; }
        if (header.types < 1 || header.types > 256) throw invalid();
        long[] transitions = new long[header.times];
        for (int i = 0; i < transitions.length; i++) {
            transitions[i] = width == 4 ? in.i32() : in.i64();
            if (i > 0 && transitions[i] <= transitions[i - 1]) throw invalid();
        }
        int[] indices = new int[header.times];
        for (int i = 0; i < indices.length; i++) { indices[i] = in.u8(); if (indices[i] >= header.types) throw invalid(); }
        int[] offsets = new int[header.types], abbreviations = new int[header.types], daylight = new int[header.types];
        for (int i = 0; i < header.types; i++) { offsets[i] = in.i32(); daylight[i] = in.u8(); abbreviations[i] = in.u8(); if (daylight[i] > 1) throw invalid(); }
        byte[] text = in.bytes(header.chars); Offset[] types = new Offset[header.types];
        for (int i = 0; i < types.length; i++) {
            int begin = abbreviations[i], end = begin;
            while (end < text.length && text[end] != 0) end++;
            if (end == text.length || begin >= text.length) throw invalid();
            types[i] = new Offset(offsets[i], SqlText.decode(Arrays.copyOfRange(text, begin, end)), daylight[i] != 0);
        }
        in.skip(Math.addExact(Math.multiplyExact(header.leaps, width + 4), header.standard + header.utc));
        Footer footer = null;
        if (width == 8 && in.position < bytes.length) {
            if (in.u8() != 10 || bytes[bytes.length - 1] != 10) throw invalid();
            String rule = SqlText.decode(in.bytes(bytes.length - in.position - 1)); in.u8();
            if (!rule.isEmpty()) footer = new Posix(rule).footer();
        }
        if (in.position != bytes.length) throw invalid();
        return new Zone(name, transitions, indices, types, footer);
    }
    private record Header(int version, int utc, int standard, int leaps, int times, int types, int chars) {
        int size(int width) { return Math.toIntExact((long) times * (width + 1) + types * 6L + chars + (long) leaps * (width + 4) + standard + utc); }
    }
    private static Header header(Reader in) {
        if (!Arrays.equals(in.bytes(4), new byte[]{'T', 'Z', 'i', 'f'})) throw invalid();
        int version = in.u8(); if (version != 0 && version != '2' && version != '3') throw invalid();
        in.skip(15);
        Header result = new Header(version, in.count(), in.count(), in.count(), in.count(), in.count(), in.count());
        if (result.size(version == 0 ? 4 : 8) > in.bytes.length) throw invalid();
        return result;
    }
    private record Rule(int month, int week, int weekday, int seconds) {
        long instant(long year, int beforeOffset) {
            long first = Temporal.days(year, month, 1);
            int date = 1 + Math.floorMod(weekday - Math.floorMod(first + 4, 7), 7) + (week - 1) * 7;
            if (date > Temporal.monthDays(year, month)) date -= 7;
            return Temporal.days(year, month, date) * 86400 + seconds - beforeOffset;
        }
    }
    private record Footer(Offset standard, Offset daylight, Rule start, Rule end) {
        Offset at(long instant) {
            if (daylight == null || start == null) return standard;
            long year = Temporal.civil(Math.floorDiv(instant, 86400)).year();
            long a = start.instant(year, standard.seconds), b = end.instant(year, daylight.seconds);
            return (a < b ? instant >= a && instant < b : instant >= a || instant < b) ? daylight : standard;
        }
    }
    private static final class Posix {
        final String text; int position;
        Posix(String text) { this.text = text; }
        boolean end() { return position == text.length(); }
        boolean take(char c) { if (!end() && text.charAt(position) == c) { position++; return true; } return false; }
        void expect(char c) { if (!take(c)) throw invalid(); }
        int number(int maximum) {
            int start = position, result = 0;
            while (!end() && text.charAt(position) >= '0' && text.charAt(position) <= '9') {
                result = Math.addExact(Math.multiplyExact(result, 10), text.charAt(position++) - '0');
                if (result > maximum) throw invalid();
            }
            if (position == start) throw invalid(); return result;
        }
        String abbreviation() {
            int start = position;
            if (take('<')) { start++; while (!end() && text.charAt(position) != '>') position++; String value = text.substring(start, position); expect('>'); if (value.isEmpty()) throw invalid(); return value; }
            while (!end() && (text.charAt(position) >= 'A' && text.charAt(position) <= 'Z' || text.charAt(position) >= 'a' && text.charAt(position) <= 'z')) position++;
            if (position - start < 3) throw invalid(); return text.substring(start, position);
        }
        int offset(int hours, int seconds) {
            boolean negative = take('-'); if (!negative) take('+');
            int result = number(hours) * 3600;
            if (take(':')) { result += number(59) * 60; if (take(':')) result += number(seconds); }
            return negative ? -result : result;
        }
        Rule rule() {
            expect('M'); int month = number(12); expect('.'); int week = number(5); expect('.'); int day = number(6);
            if (month == 0 || week == 0) throw invalid();
            return new Rule(month, week, day, take('/') ? offset(167, 60) : 7200);
        }
        Footer footer() {
            String std = abbreviation(); int standard = -offset(167, 60);
            Offset first = new Offset(standard, std, false);
            if (end()) return new Footer(first, null, null, null);
            String dst = abbreviation(); int daylight = end() || text.charAt(position) == ',' ? standard + 3600 : -offset(167, 60);
            Rule start = null, finish = null;
            if (take(',')) { start = rule(); expect(','); finish = rule(); }
            if (!end()) throw invalid();
            return new Footer(first, new Offset(daylight, dst, true), start, finish);
        }
    }
    private static final class Reader {
        final byte[] bytes; int position;
        Reader(byte[] bytes) { this.bytes = bytes; }
        void skip(int count) { if (count < 0 || count > bytes.length - position) throw invalid(); position += count; }
        byte[] bytes(int count) { int start = position; skip(count); return Arrays.copyOfRange(bytes, start, position); }
        int u8() { if (position == bytes.length) throw invalid(); return bytes[position++] & 255; }
        int u16() { return u8() << 8 | u8(); }
        int i32() { int start = position; skip(4); return ByteBuffer.wrap(bytes, start, 4).getInt(); }
        long i64() { int start = position; skip(8); return ByteBuffer.wrap(bytes, start, 8).getLong(); }
        int count() { int value = i32(); if (value < 0 || value > bytes.length) throw invalid(); return value; }
        String string() { return SqlText.decode(bytes(u16())); }
    }
    private static SqlException invalid() { return new SqlException("XX001", "invalid time zone data"); }
}
