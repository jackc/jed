// Generated data from 3e5962f9affc06c768df1688765162a65a893848.
// Regenerate only with mise run java-native:codegen.
package org.jed;

final class EstimatorFacts {
    private EstimatorFacts() {}
    record Fraction(long numerator, long denominator) {}
    static final long SCHEMA_VERSION = 1L;
    static final long MAX_ESTIMATE = 9223372036854775807L;
    static final long MAX_SELECTIVITY_DENOMINATOR = 1000000L;
    static final long JOIN_DP_LIMIT = 8L;
    static final long DEFAULT_DISTINCT_VALUES = 200L;
    static final long DEFAULT_SRF_ROWS = 1000L;
    static final long DEFAULT_VARIABLE_KEY_BYTES = 1L;
    static final long DEFAULT_ARRAY_ELEMENTS = 10L;
    static final long STATISTICS_TARGET = 100L;
    static final long STATISTICS_SAMPLE_ROWS = 30000L;
    static final long STATISTICS_KMV_HASHES = 4096L;
    static final long STATISTICS_MCV_ENTRIES = 100L;
    static final long STATISTICS_HISTOGRAM_BOUNDS = 101L;
    static final long STATISTICS_MAX_VALUE_BYTES = 128L;
    static final long STATISTICS_NDV_SCALE_NUMERATOR = 1L;
    static final long STATISTICS_NDV_SCALE_DENOMINATOR = 10L;
    static final long STATISTICS_TERM_ENTRIES = 100L;
    static final long STATISTICS_TERM_BUCKET_WIDTH = 10000L;
    static final Fraction EQUALITY = new Fraction(1L, 200L);
    static final Fraction INEQUALITY = new Fraction(1L, 3L);
    static final Fraction PAIRED_RANGE = new Fraction(1L, 200L);
    static final Fraction NULL_TEST = new Fraction(1L, 200L);
    static final Fraction MATCH = new Fraction(1L, 200L);
    static final Fraction MATCHING = new Fraction(1L, 100L);
    static final Fraction BOOLEAN = new Fraction(1L, 2L);
    static final Fraction OPAQUE = new Fraction(1L, 3L);
}
