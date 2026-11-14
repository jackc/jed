package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.Objects;

/** Trusted host code. Kernels must be safe for concurrent calls from independent sessions. */
public final class HostFunction {
    public enum Volatility { IMMUTABLE, STABLE, VOLATILE }
    @FunctionalInterface public interface ScalarKernel { SqlValue apply(List<SqlValue> arguments); }
    @FunctionalInterface public interface BatchKernel { void apply(List<List<SqlValue>> columns, List<SqlValue> output); }
    private final String name, componentId;
    private final List<SqlType> arguments;
    private final SqlType result;
    private final BatchKernel kernel;
    private final boolean batched, crossCore;
    private final long cost, semanticVersion;
    private final Volatility volatility;
    private HostFunction(String name, List<SqlType> arguments, SqlType result, BatchKernel kernel, boolean batched,
                         long cost, Volatility volatility, boolean crossCore, String componentId, long semanticVersion) {
        SqlText.validate(name); this.name = name; this.arguments = List.copyOf(arguments); this.result = Objects.requireNonNull(result);
        for (SqlType type : arguments) scalarType(type); scalarType(result);
        if (cost < 0 || semanticVersion < 0 || semanticVersion > 0xffff_ffffL) throw new SqlException("22023", "invalid host function cost or version");
        if (componentId != null) SqlText.validate(componentId);
        this.kernel = Objects.requireNonNull(kernel); this.batched = batched; this.cost = cost; this.volatility = Objects.requireNonNull(volatility);
        this.crossCore = crossCore; this.componentId = componentId; this.semanticVersion = semanticVersion;
    }
    public static HostFunction scalar(String name, List<SqlType> arguments, SqlType result, ScalarKernel kernel) {
        Objects.requireNonNull(kernel);
        return new HostFunction(name, arguments, result, (columns, output) -> {
            int count = columns.isEmpty() ? 1 : columns.getFirst().size();
            for (int row = 0; row < count; row++) { var values = new ArrayList<SqlValue>(columns.size()); for (var column : columns) values.add(column.get(row)); output.add(kernel.apply(List.copyOf(values))); }
        }, false, 1, Volatility.VOLATILE, false, null, 0);
    }
    public static HostFunction batched(String name, List<SqlType> arguments, SqlType result, BatchKernel kernel) { return new HostFunction(name, arguments, result, kernel, true, 1, Volatility.VOLATILE, false, null, 0); }
    public HostFunction cost(long value) { return copy(value, volatility, crossCore, componentId, semanticVersion); }
    public HostFunction volatility(Volatility value) { return copy(cost, value, crossCore, componentId, semanticVersion); }
    public HostFunction crossCore(boolean value) { return copy(cost, volatility, value, componentId, semanticVersion); }
    public HostFunction component(String identity, long version) { return copy(cost, volatility, crossCore, Objects.requireNonNull(identity), version); }
    private HostFunction copy(long weight, Volatility stability, boolean cross, String identity, long version) { return new HostFunction(name, arguments, result, kernel, batched, weight, stability, cross, identity, version); }
    public String name() { return name; }
    public List<SqlType> arguments() { return arguments; }
    public SqlType result() { return result; }
    public long cost() { return cost; }
    public Volatility volatility() { return volatility; }
    public boolean crossCore() { return crossCore; }
    public String componentId() { return componentId; }
    public long semanticVersion() { return semanticVersion; }
    public boolean batched() { return batched; }
    BatchKernel kernel() { return kernel; }
    private static void scalarType(SqlType type) {
        if (type == null || type == SqlType.UNKNOWN || type == SqlType.JSONPATH || type.kind() == SqlType.Kind.ARRAY || type.kind() == SqlType.Kind.RANGE || type.isComposite()) throw new SqlException("0A000", "host functions require scalar argument and result types");
        SqlType bare = SqlType.named(type.kind().name().toLowerCase(java.util.Locale.ROOT));
        if (!bare.equals(type)) throw new SqlException("0A000", "host function signatures do not carry type modifiers");
    }
}
