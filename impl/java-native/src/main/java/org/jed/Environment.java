package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Immutable execution data supplied by a privileged host before opening a database. */
public final class Environment {
    private final List<HostFunction> functions;
    public List<HostFunction> functions() { return functions; }
    public HostFunction exactFunction(String name, List<SqlType> arguments) { for (HostFunction function : functions) if (function.name().equals(name) && function.arguments().equals(arguments)) return function; return null; }
    private final List<TimeZoneData> timeZones;
    private final List<UnicodeData> unicode;
    private final java.util.function.Consumer<byte[]> random;
    private final java.util.function.LongSupplier clock;
    private final java.util.function.Supplier<java.util.function.Consumer<byte[]>> keyEntropy;
    private Environment(Builder builder) { this.functions = List.copyOf(builder.functions); this.unicode = List.copyOf(builder.unicode); this.timeZones = List.copyOf(builder.timeZones); random = builder.random; clock = builder.clock; keyEntropy = builder.keyEntropy; }
    public static Environment empty() { return builder().build(); }
    public static Builder builder() { return new Builder(); }
    public Builder toBuilder() { Builder builder = new Builder(); builder.functions.addAll(functions); builder.unicode.addAll(unicode); builder.timeZones.addAll(timeZones); builder.random = random; builder.clock = clock; builder.keyEntropy = keyEntropy; return builder; }
    public static final class Builder {
        private final List<HostFunction> functions = new ArrayList<>();
        public Builder registerFunction(HostFunction function) {
            java.util.Objects.requireNonNull(function);
            for (HostFunction prior : functions) if (prior.name().equals(function.name()) && prior.arguments().equals(function.arguments())) throw new SqlException("42723", "duplicate host function signature: " + function.name());
            functions.add(function); return this;
        }
        private final List<TimeZoneData> timeZones = new ArrayList<>();
        private final List<UnicodeData> unicode = new ArrayList<>();
        public Builder loadUnicodeData(byte[] bytes) {
            UnicodeData data = UnicodeData.load(bytes);
            if (!unicode.isEmpty() && (!unicode.getFirst().unicodeVersion().equals(data.unicodeVersion()) || !unicode.getFirst().cldrVersion().equals(data.cldrVersion()))) throw new SqlException("22023", "loaded Unicode versions differ");
            unicode.add(data); return this;
        }
        private java.util.function.Consumer<byte[]> random = new java.security.SecureRandom()::nextBytes;
        private java.util.function.LongSupplier clock = () -> { java.time.Instant now = java.time.Instant.now(); return Math.addExact(Math.multiplyExact(now.getEpochSecond(), 1_000_000L), now.getNano() / 1000); };
        private java.util.function.Supplier<java.util.function.Consumer<byte[]>> keyEntropy = () -> new java.security.SecureRandom()::nextBytes;
        public Builder loadTimeZoneData(byte[] bytes) { timeZones.add(TimeZoneData.load(bytes)); return this; }
        public Builder randomSource(java.util.function.Consumer<byte[]> source) { random = java.util.Objects.requireNonNull(source); return this; }
        public Builder clockSource(java.util.function.LongSupplier source) { clock = java.util.Objects.requireNonNull(source); return this; }
        public Builder keyEntropy(java.util.function.Supplier<java.util.function.Consumer<byte[]>> streams) { keyEntropy = java.util.Objects.requireNonNull(streams); return this; }
        public Environment build() { return new Environment(this); }
    }
    public Collation collation(String name) {
        if (name.equals("C")) return Collation.C;
        for (UnicodeData data : unicode) { Collation found = data.collation(name); if (found != null) return found; }
        throw new SqlException("42704", "collation does not exist: " + name);
    }
    public List<String> loadedCollations() {
        java.util.TreeSet<String> names = new java.util.TreeSet<>(SqlText::compare); names.add("C");
        for (UnicodeData data : unicode) names.addAll(data.names()); return List.copyOf(names);
    }
    String caseText(String text, boolean upper) {
        for (UnicodeData data : unicode) if (data.hasProperties()) return upper ? data.upper(text) : data.lower(text);
        return UnicodeData.asciiCase(text, upper);
    }
    public java.util.function.Consumer<byte[]> newKeyEntropyStream() { return java.util.Objects.requireNonNull(keyEntropy.get()); }
    void random(byte[] bytes) { random.accept(bytes); }
    long clock() { return clock.getAsLong(); }
    public TimeZoneData.Zone zone(String name) { return zone(name, false); }
    TimeZoneData.Zone zone(String name, boolean iso) {
        TimeZoneData.Zone builtin = TimeZoneData.builtin(name, iso);
        if (builtin != null) return builtin;
        for (TimeZoneData data : timeZones) { TimeZoneData.Zone result = data.named(name); if (result != null) return result; }
        throw new SqlException("22023", "time zone not recognized: " + name);
    }
    IndexDependencies.ZonePin zonePin(String name) {
        if (TimeZoneData.builtin(name, false) != null) return null;
        for (TimeZoneData data : timeZones) { IndexDependencies.ZonePin pin = data.pin(name); if (pin != null) return pin; }
        throw new SqlException("22023", "time zone not recognized: " + name);
    }
    List<IndexDependencies.ZonePin> zonePins() {
        java.util.TreeMap<String, IndexDependencies.ZonePin> pins = new java.util.TreeMap<>(SqlText::compare);
        for (TimeZoneData data : timeZones) for (String name : data.names()) pins.putIfAbsent(name, data.pin(name));
        return List.copyOf(pins.values());
    }
    public List<String> loadedTimeZones() {
        java.util.TreeSet<String> names = new java.util.TreeSet<>();
        for (TimeZoneData data : timeZones) names.addAll(data.names());
        return List.copyOf(names);
    }
}
