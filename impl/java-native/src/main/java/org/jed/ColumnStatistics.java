package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.List;
import java.util.PriorityQueue;
import java.util.TreeMap;
import java.util.TreeSet;
import org.jed.storage.PersistentTree;

/** Immutable bounded statistics facts and deterministic whole-population collection. */
record ColumnStatistics(long analyzedRows, boolean stale, long nullCount, long widthSum, Long distinctCount,
                        int sampleRows, int sampleNonnullRows, List<Mcv> mcv, List<SqlValue> histogram,
                        Terms terms) {
    record Mcv(SqlValue value, int frequency) {}
    record Term(byte[] bytes, long rows) { Term { bytes = bytes.clone(); } @Override public byte[] bytes() { return bytes.clone(); } }
    record Terms(long occurrences, long untrackedRows, List<Term> entries) { Terms { entries = List.copyOf(entries); } }
    ColumnStatistics { mcv = List.copyOf(mcv); histogram = List.copyOf(histogram); }
    ColumnStatistics markStale() { return stale ? this : new ColumnStatistics(analyzedRows, true, nullCount, widthSum, distinctCount, sampleRows, sampleNonnullRows, mcv, histogram, terms); }
    Long averageWidth() { long population = analyzedRows - nullCount; return population == 0 ? null : widthSum / population + (widthSum % population == 0 ? 0 : 1); }
    static boolean eligible(SqlType type) { return switch(type.kind()) { case UNKNOWN, ARRAY, COMPOSITE, RECORD, JSON, JSONB, JSONPATH -> false; default -> true; }; }
    static boolean termEligible(SqlType type) { try { GinIndex.validateType(type); return true; } catch (SqlException error) { return false; } }
    static byte[] key(SqlValue value, Engine.Column column, CostMeter meter) {
        if (value.type().kind() == SqlType.Kind.TEXT && !column.collation().equals("C")) return meter.unmetered(() -> CollationDerivation.key(value, meter.environment().collation(column.collation()), meter));
        return Encoding.key(value);
    }
    private record Sample(long priority, long ordinal, SqlValue value, byte[] key, boolean oversized) {}
    private record Group(SqlValue value, byte[] key, int frequency) {}
    private static final Comparator<Sample> PRIORITY = (a,b) -> { int order = Long.compareUnsigned(a.priority(), b.priority()); return order == 0 ? Long.compare(a.ordinal(), b.ordinal()) : order; };
    static ColumnStatistics collect(Engine.Table table, int column, SqlValue[] args, QueryEnvironment environment) throws IOException {
        CostMeter meter = environment.meter(); Engine.Column definition = table.columns().get(column);
        boolean eligible = eligible(definition.type()), termEligible = termEligible(definition.type());
        TreeSet<Long> hashes = new TreeSet<>(Long::compareUnsigned); boolean approximate = false;
        PriorityQueue<Sample> samples = new PriorityQueue<>(PRIORITY.reversed()); TermCounter terms = termEligible ? new TermCounter() : null;
        long population = 0, nulls = 0, width = 0; boolean[] touched = new boolean[table.columns().size()]; touched[column] = true;
        try (TableScan scan = new TableScan(table, null, table.columns().stream().map(Engine.Column::name).toList(), null, args, meter, touched, environment)) {
            PersistentTree.Entry entry;
            while ((entry = scan.next()) != null) {
                meter.charge(Costs.STORAGE_ROW_READ); meter.guard(); SqlValue value = Encoding.row(entry, table.types()).get(column);
                byte[] encoded = value.isNull() ? null : Encoding.value(value), key = value.isNull() || !eligible ? null : key(value, definition, meter);
                long size = value.isNull() ? 1 : eligible ? key.length : encoded.length - 1;
                meter.charge(Math.max(1, size) * Costs.STATISTICS_VALUE); meter.guard();
                if (value.isNull()) nulls++; else width = PlanEstimate.add(width, size);
                if (key != null) {
                    long hash = hash(key);
                    if (!hashes.contains(hash)) {
                        if (hashes.size() == EstimatorFacts.STATISTICS_KMV_HASHES) { approximate = true; if (Long.compareUnsigned(hash, hashes.last()) < 0) { hashes.pollLast(); hashes.add(hash); } }
                        else hashes.add(hash);
                    }
                }
                long priority = hash(entry.key());
                if (samples.size() < EstimatorFacts.STATISTICS_SAMPLE_ROWS || PRIORITY.compare(new Sample(priority, population, null, null, false), samples.peek()) < 0) {
                    boolean oversized = !value.isNull() && (encoded.length > EstimatorFacts.STATISTICS_MAX_VALUE_BYTES || key != null && key.length > EstimatorFacts.STATISTICS_MAX_VALUE_BYTES);
                    Sample sample = new Sample(priority, population, oversized ? null : value, oversized ? null : key, oversized);
                    if (samples.size() == EstimatorFacts.STATISTICS_SAMPLE_ROWS) samples.remove(); samples.add(sample);
                }
                if (terms != null && !value.isNull()) for (byte[] term : GinIndex.terms(value)) { meter.charge(Math.max(1, term.length) * Costs.STATISTICS_VALUE); meter.guard(); terms.add(term); }
                population++;
            }
        }
        long nonnull = population - nulls;
        Long distinct = !eligible ? null : approximate ? Math.min(nonnull, Math.max(EstimatorFacts.STATISTICS_KMV_HASHES + 1, kmv(hashes.last()))) : (long)hashes.size();
        List<Sample> ordered = new ArrayList<>(); int sampleNonnull = 0; boolean oversized = false;
        for (Sample sample : samples) { if (sample.oversized() || sample.value() != null && !sample.value().isNull()) sampleNonnull++; oversized |= sample.oversized(); if (!sample.oversized() && sample.key() != null) ordered.add(sample); }
        ordered.sort((a,b) -> Arrays.compareUnsigned(a.key(), b.key()));
        List<Group> groups = new ArrayList<>();
        for (int at = 0; at < ordered.size();) { int end = at + 1; while (end < ordered.size() && Arrays.equals(ordered.get(at).key(), ordered.get(end).key())) end++; Sample sample = ordered.get(at); groups.add(new Group(sample.value(), sample.key(), end - at)); at = end; }
        boolean complete = samples.size() == population && !oversized && groups.size() <= EstimatorFacts.STATISTICS_MCV_ENTRIES;
        List<Group> selected = new ArrayList<>();
        if (eligible) for (Group group : groups) if (complete || group.frequency() >= 2 && PlanEstimate.mul(group.frequency(), distinct) > sampleNonnull) selected.add(group);
        selected.sort((a,b) -> { int order = Integer.compare(b.frequency(), a.frequency()); return order == 0 ? Arrays.compareUnsigned(a.key(), b.key()) : order; });
        if (selected.size() > EstimatorFacts.STATISTICS_MCV_ENTRIES) selected.subList((int)EstimatorFacts.STATISTICS_MCV_ENTRIES, selected.size()).clear();
        List<Mcv> mcv = selected.stream().map(group -> new Mcv(group.value(), group.frequency())).toList(); List<SqlValue> histogram = new ArrayList<>();
        if (eligible && !oversized) {
            List<Sample> remainder = ordered.stream().filter(sample -> selected.stream().noneMatch(group -> Arrays.equals(group.key(), sample.key()))).toList();
            int count = Math.min((int)EstimatorFacts.STATISTICS_HISTOGRAM_BOUNDS, remainder.size());
            if (count >= 2) for (int i = 0; i < count; i++) histogram.add(remainder.get((int)((long)i * (remainder.size() - 1) / (count - 1))).value());
        }
        return new ColumnStatistics(population, false, nulls, width, distinct, samples.size(), sampleNonnull, mcv, histogram, terms == null ? null : terms.finish(nonnull));
    }
    static long hash(byte[] bytes) { long value = 0xcbf29ce484222325L; for (byte b : bytes) value = (value ^ (b & 255L)) * 0x100000001b3L; value = (value ^ (value >>> 30)) * 0xbf58476d1ce4e5b9L; value = (value ^ (value >>> 27)) * 0x94d049bb133111ebL; return value ^ (value >>> 31); }
    private static long kmv(long maximum) {
        long divisor = maximum + 1, numerator = EstimatorFacts.STATISTICS_KMV_HASHES - 1;
        if (divisor == 0) return numerator;
        long remainder = 0, quotient = 0;
        for (int bit = 127; bit >= 0; bit--) {
            boolean carry = remainder < 0; remainder = (remainder << 1) | (bit >= 64 ? (numerator >>> (bit - 64)) & 1 : 0);
            if (carry || Long.compareUnsigned(remainder, divisor) >= 0) { remainder -= divisor; if (bit >= 63) return Long.MAX_VALUE; quotient |= 1L << bit; }
        }
        return remainder == 0 ? quotient : PlanEstimate.add(quotient, 1);
    }
    private static final class TermCounter {
        private static final class Count { long count, delta; Count(long count, long delta) { this.count = count; this.delta = delta; } }
        private final TreeMap<byte[],Count> counts = new TreeMap<>(Arrays::compareUnsigned);
        private long occurrences; private boolean oversized, pruned;
        void add(byte[] term) {
            occurrences = PlanEstimate.add(occurrences, 1); long bucket = (occurrences - 1) / EstimatorFacts.STATISTICS_TERM_BUCKET_WIDTH + 1;
            if (term.length > EstimatorFacts.STATISTICS_MAX_VALUE_BYTES) oversized = true;
            else { Count count = counts.get(term); if (count == null) counts.put(term, new Count(1, bucket - 1)); else count.count = PlanEstimate.add(count.count, 1); }
            if (occurrences % EstimatorFacts.STATISTICS_TERM_BUCKET_WIDTH == 0) pruned |= counts.entrySet().removeIf(entry -> PlanEstimate.add(entry.getValue().count, entry.getValue().delta) <= bucket);
        }
        Terms finish(long population) {
            List<Term> ordered = counts.entrySet().stream().map(entry -> new Term(entry.getKey(), entry.getValue().count)).sorted((a,b) -> { int order = Long.compare(b.rows(), a.rows()); return order == 0 ? Arrays.compareUnsigned(a.bytes(), b.bytes()) : order; }).toList();
            int cap = (int)EstimatorFacts.STATISTICS_TERM_ENTRIES; long untracked = ordered.size() > cap ? ordered.get(cap).rows() : 0;
            if (pruned) untracked = Math.max(untracked, (occurrences - 1) / EstimatorFacts.STATISTICS_TERM_BUCKET_WIDTH + 1);
            if (oversized) untracked = Math.max(untracked, 1);
            return new Terms(occurrences, Math.min(population, untracked), ordered.subList(0, Math.min(cap, ordered.size())));
        }
    }
}
