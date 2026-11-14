package org.jed;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;

/** Canonical kind-four catalog records; each bounded item is independent of page packing. */
final class StatisticsCodec {
    private StatisticsCodec() {}
    static List<byte[]> encode(Engine.Snapshot snapshot) {
        List<byte[]> result = new ArrayList<>();
        for (Engine.Table table : CatalogCodec.ordered(snapshot)) for (var entry : table.statistics().entrySet()) {
            int column = entry.getKey(); ColumnStatistics value = entry.getValue(); Encoding.Sink out = header(0, table.name(), column);
            out.u8((value.stale() ? 1 : 0) | (value.distinctCount() == null ? 0 : 2) | (value.terms() == null ? 0 : 4));
            out.unsigned(value.analyzedRows(), 8); out.unsigned(value.nullCount(), 8); out.unsigned(value.widthSum(), 8); out.unsigned(value.distinctCount() == null ? 0 : value.distinctCount(), 8);
            out.unsigned(value.sampleRows(), 4); out.unsigned(value.sampleNonnullRows(), 4); out.u16(value.mcv().size()); out.u16(value.histogram().size());
            if (value.terms() != null) { out.unsigned(value.terms().occurrences(), 8); out.unsigned(value.terms().untrackedRows(), 8); out.u16(value.terms().entries().size()); }
            result.add(out.bytes());
            for (int i = 0; i < value.mcv().size(); i++) { out = header(1, table.name(), column); out.u16(i); out.unsigned(value.mcv().get(i).frequency(), 4); out.sized(Encoding.value(value.mcv().get(i).value())); result.add(out.bytes()); }
            for (int i = 0; i < value.histogram().size(); i++) { out = header(2, table.name(), column); out.u16(i); out.sized(Encoding.value(value.histogram().get(i))); result.add(out.bytes()); }
            if (value.terms() != null) for (int i = 0; i < value.terms().entries().size(); i++) { ColumnStatistics.Term term = value.terms().entries().get(i); out = header(3, table.name(), column); out.u16(i); out.unsigned(term.rows(), 8); out.sized(term.bytes()); result.add(out.bytes()); }
        }
        return result;
    }
    private static Encoding.Sink header(int kind, String name, int column) { Encoding.Sink out = new Encoding.Sink(); out.u8(4); out.u8(kind); out.sized(SqlText.encode(name)); out.u16(column); return out; }
    static final class Decoder {
        private String tableName; private int column = -1, previousKind;
        private Engine.Column definition;
        private long population, nulls, width, distinct, occurrences, untracked;
        private int flags, sampled, nonnull, mcvCount, histogramCount, termCount;
        private final List<ColumnStatistics.Mcv> mcv = new ArrayList<>();
        private final List<SqlValue> histogram = new ArrayList<>();
        private final List<ColumnStatistics.Term> terms = new ArrayList<>();
        void read(Encoding.Reader in, Map<String,Engine.Table> tables) {
            int kind = in.u8(); String name = SqlText.decode(in.sized()); int ordinal = in.u16();
            Engine.Table table = tables.get(name);
            if (kind > 3 || table == null || ordinal >= table.columns().size()) throw corrupt("invalid statistics owner or kind");
            if (kind == 0) {
                if (tableName != null && (SqlText.compare(name, tableName) < 0 || name.equals(tableName) && ordinal <= column)) throw corrupt("statistics summary order");
                finish(tables); tableName = name; column = ordinal; definition = table.columns().get(column); previousKind = 0;
                flags = in.u8(); population = nonnegative(in.unsigned(8)); nulls = nonnegative(in.unsigned(8)); width = nonnegative(in.unsigned(8)); distinct = nonnegative(in.unsigned(8));
                long sample = in.unsigned(4), sampleNonnull = in.unsigned(4); mcvCount = in.u16(); histogramCount = in.u16();
                termCount = 0; occurrences = 0; untracked = 0;
                if ((flags & 4) != 0) { occurrences = nonnegative(in.unsigned(8)); untracked = nonnegative(in.unsigned(8)); termCount = in.u16(); }
                boolean eligible = ColumnStatistics.eligible(definition.type());
                if ((flags & ~7) != 0 || ((flags & 2) != 0) != eligible || ((flags & 4) != 0) != ColumnStatistics.termEligible(definition.type())
                    || nulls > population || distinct > population - nulls || !eligible && (distinct != 0 || mcvCount != 0 || histogramCount != 0)
                    || sample > Math.min(population, EstimatorFacts.STATISTICS_SAMPLE_ROWS) || sampleNonnull > sample
                    || mcvCount > EstimatorFacts.STATISTICS_MCV_ENTRIES || histogramCount > EstimatorFacts.STATISTICS_HISTOGRAM_BOUNDS || histogramCount == 1
                    || termCount > EstimatorFacts.STATISTICS_TERM_ENTRIES || untracked > population - nulls) throw corrupt("invalid statistics summary");
                sampled = (int)sample; nonnull = (int)sampleNonnull; return;
            }
            if (!name.equals(tableName) || ordinal != column || kind < previousKind) throw corrupt("statistics item order"); previousKind = kind;
            int item = in.u16();
            if (kind == 3) {
                long count = nonnegative(in.unsigned(8)); byte[] bytes = in.sized();
                if ((flags & 4) == 0 || item != terms.size() || item >= termCount || count == 0 || count > population - nulls || bytes.length == 0 || bytes.length > EstimatorFacts.STATISTICS_MAX_VALUE_BYTES) throw corrupt("invalid statistics term");
                if (!terms.isEmpty()) { ColumnStatistics.Term previous = terms.getLast(); if (count > previous.rows() || count == previous.rows() && Arrays.compareUnsigned(previous.bytes(), bytes) >= 0) throw corrupt("unordered statistics terms"); }
                if (terms.stream().anyMatch(term -> Arrays.equals(term.bytes(), bytes))) throw corrupt("duplicate statistics term");
                terms.add(new ColumnStatistics.Term(bytes, count)); return;
            }
            long frequency = kind == 1 ? in.unsigned(4) : 0; byte[] bytes = in.sized();
            if (bytes.length == 0 || bytes.length > EstimatorFacts.STATISTICS_MAX_VALUE_BYTES) throw corrupt("oversized statistics value");
            Encoding.Reader valueBytes = new Encoding.Reader(bytes); SqlValue value = Encoding.read(valueBytes, definition.type());
            if (value.isNull() || valueBytes.remaining() != 0 || !Arrays.equals(bytes, Encoding.value(value))) throw corrupt("noncanonical statistics value");
            byte[] key = Encoding.key(value); if (key.length > EstimatorFacts.STATISTICS_MAX_VALUE_BYTES) throw corrupt("oversized statistics key");
            boolean comparable = definition.collation().equals("C");
            if (kind == 1) {
                if (item != mcv.size() || item >= mcvCount || frequency == 0 || frequency > nonnull) throw corrupt("invalid statistics MCV ordinal or frequency");
                if (!mcv.isEmpty()) { ColumnStatistics.Mcv previous = mcv.getLast(); if (frequency > previous.frequency() || comparable && frequency == previous.frequency() && Arrays.compareUnsigned(Encoding.key(previous.value()), key) >= 0) throw corrupt("unordered MCV values"); }
                if (comparable && mcv.stream().anyMatch(itemValue -> Arrays.equals(Encoding.key(itemValue.value()), key))) throw corrupt("duplicate MCV value");
                mcv.add(new ColumnStatistics.Mcv(value, (int)frequency));
            } else {
                if (item != histogram.size() || item >= histogramCount || comparable && !histogram.isEmpty() && Arrays.compareUnsigned(Encoding.key(histogram.getLast()), key) > 0) throw corrupt("invalid histogram order");
                histogram.add(value);
            }
        }
        void finish(Map<String,Engine.Table> tables) {
            if (tableName == null) return;
            if (mcv.size() != mcvCount || histogram.size() != histogramCount || terms.size() != termCount || mcv.stream().mapToLong(ColumnStatistics.Mcv::frequency).sum() > nonnull) throw corrupt("incomplete statistics items");
            ColumnStatistics value = new ColumnStatistics(population, (flags & 1) != 0, nulls, width, (flags & 2) == 0 ? null : distinct, sampled, nonnull, mcv, histogram, (flags & 4) == 0 ? null : new ColumnStatistics.Terms(occurrences, untracked, terms));
            Engine.Table table = tables.get(tableName); var facts = new TreeMap<>(table.statistics()); if (facts.put(column, value) != null) throw corrupt("duplicate statistics summary"); tables.put(tableName, table.withStatistics(facts));
            mcv.clear(); histogram.clear(); terms.clear(); tableName = null;
        }
    }
    private static long nonnegative(long value) { if (value < 0) throw corrupt("negative statistics count"); return value; }
    private static SqlException corrupt(String message) { return new SqlException("XX001", message); }
}
