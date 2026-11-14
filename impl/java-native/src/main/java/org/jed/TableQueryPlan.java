package org.jed;

import java.io.IOException;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import org.jed.storage.PersistentTree;

/** Streaming table projection with bounded external ordering and schema-based key lookup. */
final class TableQueryPlan implements QueryPlan {
    private final Engine.Table table;
    private final SqlParser.Select querySyntax;
    private List<SqlParser.Expr> projectedSyntaxMetadata;
    private List<ScalarPlan.Binding> bindingMetadata;
    private final ScalarPlan.Frame frame = new ScalarPlan.Frame();
    private final List<String> labels = new ArrayList<>();
    private final List<ScalarPlan.Expression> projections = new ArrayList<>();
    private final List<ScalarPlan.Expression> orderKeys = new ArrayList<>();
    private final List<SqlParser.Order> orders;
    private final ScalarPlan.Expression where, limit, offset;
    private final SqlParser.Expr predicateSyntax;
    private final SqlValue[] arguments;
    private final String sourceAlias;
    private final List<String> sourceNames;
    private final ScalarPlan.Resolver resolver;
    private final QueryEnvironment environment;
    private final long workMem;
    private final Path scratch;
    private final boolean sort, reverse;
    private final Engine.Index orderedIndex;
    private final boolean storageOrder;
    private final boolean hostAttachment;
    private final boolean distinct;
    private final DistinctOn distinctOn;
    private final List<Integer> orderProjections = new ArrayList<>();
    private DistinctState seen;
    private TableScan scan;
    private boolean streamingDistinct;
    private RowSpool collectingResults;
    private boolean collectedCurrent;
    private final PlanMetrics metrics = new PlanMetrics();
    public void requireAnalyze() {}
    public PlanNode actualPlan(boolean verbose) { return metrics.render(explain(verbose)); }
    private ExternalSorter sorter;
    private ResidentSort residentSort;
    private RowSpool.Cursor sorted;
    private RowSpool distinctRows;
    private HostBatch.Window hostWindow;
    private List<List<SqlValue>> gathered;
    private int gatheredAt;
    private MemoryAccount gatheredAccount;
    private long gatheredBytes;
    private boolean started, eagerDistinct;
    private long remaining, skip;

    TableQueryPlan(SqlParser.Select query, Engine.Table table, SqlParser.Table source, SqlValue[] args, long workMem, Path scratch) {
        this(query, table, source, args, workMem, scratch, null);
    }
    TableQueryPlan(SqlParser.Select query, Engine.Table table, SqlParser.Table source, SqlValue[] args, long workMem, Path scratch, QueryEnvironment environment) {
        this.environment = environment; this.querySyntax = query; hostAttachment = DomainRouting.hostAttachment(source.name());

        this.table = table; this.workMem = workMem; this.scratch = scratch;
        String alias = source.alias() == null ? table.name() : source.alias();
        sourceAlias = alias; arguments = args; predicateSyntax = query.where();
        if (source.columns().size() > table.columns().size()) throw new SqlException("42P10", "too many table column aliases");
        List<ScalarPlan.Binding> bindings = new ArrayList<>();
        List<String> columnNames = new ArrayList<>();
        for (int c = 0; c < table.columns().size(); c++) {
            Engine.Column column = table.columns().get(c);
            String name = c < source.columns().size() ? source.columns().get(c) : column.name(); columnNames.add(name);
            bindings.add(new ScalarPlan.Binding(List.of(alias), name, column.type(), false, false, CollationDerivation.collatable(column.type()) ? CollationDerivation.implicit(column.collation()) : CollationDerivation.NONE).catalog(column.name(), column.notNull()));
        }
        sourceNames = List.copyOf(columnNames); bindingMetadata = List.copyOf(bindings);
        if (environment != null) environment.captureStars(query.items(), bindings);
        resolver = environment == null ? new ScalarPlan.Resolver(args, bindings, frame) : environment.resolver(bindings, frame);
        List<SqlParser.Expr> projectedSyntax = new ArrayList<>();
        for (SqlParser.Item item : ProjectionSyntax.expand(query.items(), resolver)) {
            if (item.expression() instanceof SqlParser.Star star) {
                if (!star.qualifier().isEmpty() && !star.qualifier().equals(List.of(alias))) throw new SqlException("42P01", "unknown star qualifier");
                for (int column = 0; column < columnNames.size(); column++) { String name = columnNames.get(column); labels.add(name); projectedSyntax.add(java.util.Collections.frequency(columnNames, name) > 1 ? new SqlParser.ColumnSlot(column) : new SqlParser.Name(List.of(alias, name))); }
            } else { labels.add(item.alias() == null ? ScalarPlan.outputName(item.expression()) : item.alias()); projectedSyntax.add(item.expression()); }
        }
        projectedSyntaxMetadata = List.copyOf(projectedSyntax);
        boolean unique = table.primaryKey().length > 0;
        for (int key : table.primaryKey()) {
            String keyName = columnNames.get(key);
            if (projectedSyntax.stream().noneMatch(expression -> expression instanceof SqlParser.Name name && name.parts().getLast().equals(keyName) && (name.parts().size() == 1 || name.parts().equals(List.of(alias, keyName))))) unique = false;
        }
        distinct = query.distinct() && query.distinctOn().isEmpty() && !unique;
        for (SqlParser.Expr expression : projectedSyntax) projections.add(resolver.resolve(expression, null));
        if (query.distinct() && query.distinctOn().isEmpty()) for (int i = 0; i < projections.size(); i++) { ScalarPlan.requireComparable(projections.get(i).type()); CollationDerivation.requireSort(projections.get(i), projectedSyntax.get(i)); }
        if (AggregatePlan.contains(query.where())) throw new SqlException("42803", "aggregate in WHERE");
        where = query.where() == null ? null : resolver.predicate(query.where());
        if (where != null && where.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("WHERE requires boolean");
        ScalarPlan.Resolver constants = (environment == null ? new ScalarPlan.Resolver(args) : environment.resolver()).excludeLocal(sourceNames, sourceAlias);
        limit = query.limit() == null ? null : constants.resolve(query.limit(), SqlType.I64);
        offset = query.offset() == null ? null : constants.resolve(query.offset(), SqlType.I64);
        if (limit != null && !limit.type().isInteger() || offset != null && !offset.type().isInteger()) throw SqlException.typeMismatch("row bound requires integer");
        distinctOn = query.distinctOn().isEmpty() ? null : new DistinctOn(query.distinctOn(), query.order(), projectedSyntax, labels, bindings, resolver);
        orders = distinctOn == null ? query.order() : distinctOn.order;
        List<SqlParser.Expr> orderedSyntax = new ArrayList<>();
        boolean elided = orders.isEmpty() || table.primaryKey().length > 0;
        boolean reversed = !orders.isEmpty() && table.primaryKey().length > 0 && orders.size() >= table.primaryKey().length;
        for (int i = 0; i < orders.size(); i++) {
            SqlParser.Order order = orders.get(i);
            SqlParser.Expr ordered = order.expression();
            if (ordered instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER) {
                int ordinal;
                try { ordinal = ScalarPlan.ordinal(literal.text()); } catch (NumberFormatException error) { throw new SqlException("42P10", "ORDER BY position out of range"); }
                if (ordinal < 1 || ordinal > projectedSyntax.size()) throw new SqlException("42P10", "ORDER BY position out of range");
                ordered = projectedSyntax.get(ordinal - 1);
            } else if (ordered instanceof SqlParser.Name name && name.parts().size() == 1 && labels.contains(name.parts().getFirst())) {
                int first = labels.indexOf(name.parts().getFirst());
                for (int j = first + 1; j < labels.size(); j++) if (labels.get(j).equals(name.parts().getFirst()) && !projectedSyntax.get(j).equals(projectedSyntax.get(first))) throw new SqlException("42702", "ambiguous ORDER BY label");
                ordered = projectedSyntax.get(first);
            }
            orderedSyntax.add(ordered);
            orderKeys.add(resolver.resolve(ordered, null)); ScalarPlan.requireComparable(orderKeys.getLast().type()); CollationDerivation.requireSort(orderKeys.getLast(), ordered);
            int projection = projectedSyntax.indexOf(ordered);
            if (distinct && projection < 0) throw new SqlException("42P10", "DISTINCT ORDER BY must appear in select list");
            orderProjections.add(projection);
            // A complete unique key leaves no ties for additional ordering terms.
            if (i >= table.primaryKey().length) continue;
            if (i >= table.primaryKey().length || !orderKeys.getLast().collation().require().equals(table.columns().get(table.primaryKey()[i]).collation())) { elided = false; reversed = false; }
            if (order.descending() || i >= table.primaryKey().length || !(ordered instanceof SqlParser.Name name)
                    || !name.parts().getLast().equals(columnNames.get(table.primaryKey()[i]))) elided = false;
            if (!order.descending() || i >= table.primaryKey().length || !(ordered instanceof SqlParser.Name name)
                    || !name.parts().getLast().equals(columnNames.get(table.primaryKey()[i]))) reversed = false;
        }
        if (environment != null && !CollationCompatibility.table(table, environment.meter())) { reversed = false; elided = false; }
        elided |= reversed;
        boolean forceStorage = false;
        if ((orders.isEmpty() && limit != null || !orders.isEmpty() && elided) && !hostAttachment && !table.indexes().isEmpty() && point(predicateSyntax, table, sourceNames, sourceAlias, constants) == null) {
            try {
                IndexAccess.Selection secondary = IndexAccess.select(table, sourceNames, sourceAlias, predicateSyntax, environment.meter(), null, constants);
                TableScan.PrimarySelection primary = TableScan.primary(table, sourceNames, sourceAlias, predicateSyntax, constants, environment.meter());
                if (primary != null && secondary != null && primary.cost() <= secondary.cost()) secondary = null;
                long incumbent = secondary != null ? secondary.cost() : primary != null ? primary.cost() : Long.MAX_VALUE;
                GistEstimate.Selection gist = GistEstimate.select(table, sourceNames, sourceAlias, predicateSyntax, constants, environment.meter(), incumbent, secondary == null && primary == null);
                if (gist != null) incumbent = gist.cost();
                GinEstimate.Selection gin = GinEstimate.select(table, sourceNames, sourceAlias, predicateSyntax, constants, environment.meter(), incumbent, false, secondary == null && primary == null && gist == null);
                if (secondary != null || gist != null || gin != null) {
                    if (limit != null && !distinct && distinctOn == null) {
                        long wanted = PlanEstimate.add(TableExplanation.boundEstimate(querySyntax.limit(), Long.MAX_VALUE, constants, environment.meter()), TableExplanation.boundEstimate(querySyntax.offset(), 0, constants, environment.meter()));
                        long pulled = requiredRows(predicateSyntax, table.rows().size(), wanted, constants);
                        long pages = pulled == 0 ? 0 : Math.min(table.rows().structuralFacts().nodeCount(), Math.max(table.rows().structuralFacts().height(), pulled));
                        long fullCost = PlanEstimate.add(pages, PlanEstimate.mul(pulled, 1 + PlanEstimate.operations(predicateSyntax)));
                        long secondaryCost = gin != null ? gin.cost() : gist != null ? gist.cost() : secondary.cost();
                        if (orders.isEmpty()) {
                            long estimatedRows = gin != null ? gin.rows() : gist != null ? gist.rows() : secondary.rows();
                            long indexRows = Math.min(estimatedRows, pulled);
                            long indexPages = Math.max(0, (gin != null ? gin.pages() : gist != null ? gist.pages() : secondary.pages()) - PlanEstimate.mul(estimatedRows, table.rows().structuralFacts().height()));
                            if (gin != null || gist != null) indexPages = PlanEstimate.add(indexPages, gin != null ? gin.entries() : gist.entries());
                            secondaryCost = PlanEstimate.add(indexPages, PlanEstimate.mul(indexRows, table.rows().structuralFacts().height() + 1 + PlanEstimate.operations(predicateSyntax)));
                        }
                        forceStorage = fullCost <= secondaryCost;
                    }
                    if (!forceStorage && gin == null && gist == null) { elided = false; reversed = false; }
                }
            } catch (IOException failure) { throw Engine.storageError(failure); }
        }
        storageOrder = forceStorage; reverse = reversed;
        orderedIndex = !hostAttachment && !elided && !orders.isEmpty() && !distinct && distinctOn == null ? orderIndex(orderedSyntax) : null;
        elided |= orderedIndex != null;
        sort = distinctOn != null || distinct && (orders.isEmpty() || !elided) || !orders.isEmpty() && !elided;
        if (environment != null) environment.viewTable(table, resolver.touched());
    }
    private long requiredRows(SqlParser.Expr predicate, long maximum, long wanted, ScalarPlan.Resolver constants) {
        StatisticsEstimate statistics = new StatisticsEstimate(table, sourceNames, sourceAlias, constants, environment.meter());
        List<String> unique = table.primaryKey().length == 1 ? List.of(sourceNames.get(table.primaryKey()[0])) : List.of();
        if (wanted == 0) return 0;
        if (statistics.rows(predicate, maximum, unique) < wanted) return maximum;
        long low = 0, high = maximum;
        while (low < high) { long middle = low + (high - low) / 2; if (statistics.rows(predicate, middle, unique) >= wanted) high = middle; else low = middle + 1; }
        return low;
    }
    private Engine.Index orderIndex(List<SqlParser.Expr> syntax) {
        for (int key : table.primaryKey()) if (table.columns().get(key).type().kind() == SqlType.Kind.DECIMAL || Encoding.fixedWidth(table.columns().get(key).type()) == 0) return null;
        return table.indexes().stream().filter(index -> {
            if (!StoredDefinitions.parsed(index) || index.kind() != 0 || index.keys().size() != orders.size() || !CollationCompatibility.index(index, environment.meter()) || (!IndexDependencies.matches(index.timezoneDependencies(), environment.meter().environment()) || !HostDependencies.matches(index.hostDependencies(), environment.meter().environment()))) return false;
            for (int i = 0; i < orders.size(); i++) {
                Engine.IndexKey key = index.keys().get(i); SqlParser.Order order = orders.get(i);
                if (key.column() < 0 || !(syntax.get(i) instanceof SqlParser.Name name) || !name.parts().getLast().equals(sourceNames.get(key.column())) || order.descending() != key.descending() || (order.nullsFirst() == null ? order.descending() : order.nullsFirst()) != key.nullsFirst() || !orderKeys.get(i).collation().require().equals(key.collation())) return false;
            }
            if (index.predicate() != null && !PredicateImplication.implies(predicateSyntax, index.predicate(), table, sourceAlias, environment)) return false;
            return orderCost(index) != Long.MAX_VALUE;
        }).min((a,b) -> {
            int cost = Long.compare(orderCost(a), orderCost(b));
            return cost != 0 ? cost : java.util.Arrays.compareUnsigned(SqlText.encode(a.name()), SqlText.encode(b.name()));
        }).orElse(null);
    }
    private long orderCost(Engine.Index index) {
        try {
            ScalarPlan.Resolver constants = environment.resolver().excludeLocal(sourceNames, sourceAlias);
            long n = table.rows().size(); var facts = table.rows().structuralFacts();
            long wanted = PlanEstimate.add(TableExplanation.boundEstimate(querySyntax.limit(), Long.MAX_VALUE, constants, environment.meter()), TableExplanation.boundEstimate(querySyntax.offset(), 0, constants, environment.meter()));
            long scans = requiredRows(predicateSyntax, n, wanted, constants);
            long operations = PlanEstimate.operations(predicateSyntax, constants, environment.meter());
            IndexAccess.Selection existing = IndexAccess.select(table, sourceNames, sourceAlias, predicateSyntax, environment.meter(), null, constants);
            long blocking = existing == null ? PlanEstimate.add(facts.nodeCount(), PlanEstimate.mul(n, 1 + operations)) : existing.cost();
            TableScan.PrimarySelection primary = TableScan.primary(table, sourceNames, sourceAlias, predicateSyntax, constants, environment.meter());
            if (primary != null) blocking = Math.min(blocking, primary.cost());
            GistEstimate.Selection gist = GistEstimate.select(table, sourceNames, sourceAlias, predicateSyntax, constants, environment.meter(), blocking, true);
            if (gist != null) blocking = gist.cost();
            GinEstimate.Selection gin = GinEstimate.select(table, sourceNames, sourceAlias, predicateSyntax, constants, environment.meter(), blocking, false, true);
            if (gin != null) blocking = gin.cost();
            KeySetEstimate.Selection keys = KeySetEstimate.primary(table, sourceNames, sourceAlias, predicateSyntax, constants, environment.meter());
            if (keys != null) blocking = Math.min(blocking, keys.cost());
            IndexAccess.Selection bound = IndexAccess.select(table, sourceNames, sourceAlias, predicateSyntax, environment.meter(), index, constants);
            if (bound == null && limit == null) return Long.MAX_VALUE;
            long pages;
            if (bound == null) pages = index.entries().structuralFacts().nodeCount();
            else { scans = Math.min(scans, bound.rows()); pages = Math.max(0, bound.pages() - PlanEstimate.mul(bound.rows(), facts.height())); }
            long ordered = PlanEstimate.add(pages, PlanEstimate.mul(scans, facts.height() + 1 + operations));
            return ordered <= blocking ? ordered : Long.MAX_VALUE;
        } catch (IOException failure) { throw Engine.storageError(failure); }
        catch (SqlException unsupported) { if (unsupported.sqlState().equals("0A000")) return Long.MAX_VALUE; throw unsupported; }
    }
    @Override public PlanNode explain(boolean verbose) {
        return TableExplanation.describe(querySyntax, table, sourceNames, sourceAlias, projectedSyntaxMetadata, bindingMetadata, resolver, environment.meter(), sort, distinct, reverse, verbose, true, true, distinctOn, orderedIndex, hostAttachment);
    }
    @Override public List<String> names() { return List.copyOf(labels); }
    @Override public List<CollationDerivation> collations() { return projections.stream().map(ScalarPlan.Expression::collation).toList(); }
    @Override public List<SqlType> types() { return projections.stream().map(ScalarPlan.Expression::type).toList(); }
    @Override public Row next(CostMeter meter) {
        long before = meter.cost();
        try { return nextMeasured(meter); } finally { metrics.total(before, meter); }
    }
    @Override public void collect(RowSpool target, CostMeter meter) {
        collectingResults = target;
        try { Row row; while ((row = next(meter)) != null) if (!collectedCurrent) target.append(row.values()); }
        finally { collectingResults = null; }
    }
    private Row nextMeasured(CostMeter meter) {
        collectedCurrent = false;
        try {
            if (!started) {
                started = true; remaining = bound(limit, meter, Long.MAX_VALUE, "2201W"); skip = bound(offset, meter, 0, "2201X");
                long scanBefore = meter.cost();
                scan = new TableScan(table, sourceAlias, sourceNames, hostAttachment ? null : predicateSyntax, arguments, meter, resolver.touched(), environment, reverse, orderedIndex, storageOrder);
                metrics.add("Scan", scanBefore, meter);
                // A bound windows the deduplicated result, not the projected input. Finish
                // the blocking lane first so a later projection failure cannot be hidden.
                streamingDistinct = distinct && orders.isEmpty() && limit == null && offset == null && scratch != null && workMem > 0;
                if (streamingDistinct) seen = new DistinctState(scratch, workMem, meter.memory());
                else if (distinct && scratch == null && sort) { eagerDistinct = true; buildEagerDistinct(meter); }
                else if (sort) buildSort(meter);
                else if (distinct) seen = new DistinctState(scratch, workMem, meter.memory());
                else if (!sort && orders.isEmpty() && limit == null && offset == null && projectedSyntaxMetadata.stream().allMatch(item -> item instanceof SqlParser.Name || item instanceof SqlParser.ColumnSlot)) gatherColumns(meter);
            }
            if (gathered != null) {
                if (gatheredAt == gathered.size()) return null;
                meter.charge(Costs.ROW_PRODUCED); meter.guard();
                return new Row(labels, gathered.get(gatheredAt++));
            }
            while (remaining > 0) {
                List<SqlValue> input = batchedInput(meter);
                if (input == null) return null;
                if (!distinct || (!sort || streamingDistinct) && !eagerDistinct) frame.values = input;
                if ((!sort || streamingDistinct) && !eagerDistinct && !matches(meter)) continue;
                if (distinctOn != null && !distinctOn.accept(input, table.columns().size())) continue;
                List<SqlValue> projected = null;
                if (distinct && (!sort || streamingDistinct) && !eagerDistinct) {
                    projected = new ArrayList<>();
                    for (ScalarPlan.Expression expression : projections) { SqlValue value = expression.run(meter); meter.charge(MemoryAccount.payload(value) * Costs.HASH_KEY); meter.guard(); projected.add(value); }
                    if (collectingResults != null && skip == 0 && !seen.contains(projected)) { collectingResults.append(projected); collectedCurrent = true; }
                    if (!seen.add(projected)) continue;
                }
                if (skip > 0) { skip--; continue; }
                remaining--; meter.charge(Costs.ROW_PRODUCED); meter.guard();
                if (distinct) return new Row(labels, sort && !streamingDistinct || eagerDistinct ? input.subList(0, projections.size()) : projected);
                List<SqlValue> result = new ArrayList<>(projections.size());
                for (ScalarPlan.Expression expression : projections) result.add(expression.run(meter));
                return new Row(labels, result);
            }
            return null;
        } catch (IOException failure) { throw Engine.storageError(failure); }
    }
    private void gatherColumns(CostMeter meter) throws IOException {
        gathered = new ArrayList<>(); gatheredAccount = meter.memory();
        List<SqlValue> values;
        while ((values = nextInput(meter)) != null) {
            frame.values = values; if (!matches(meter)) continue;
            List<SqlValue> projected = new ArrayList<>();
            for (ScalarPlan.Expression expression : projections) projected.add(expression.run(meter));
            long bytes = DistinctState.keyBytes(projected); gatheredAccount.reserve(bytes); gatheredBytes = MemoryAccount.add(gatheredBytes, bytes);
            gathered.add(projected);
        }
    }
    private List<SqlValue> batchedInput(CostMeter meter) throws IOException {
        if (eagerDistinct) return sorted.next();
        if (resolver.hostCalls.isEmpty() || distinct || !sort && !scan.canReadAhead()) return sort && !streamingDistinct ? sorted.next() : nextInput(meter);
        if (hostWindow == null) {
            var roots = new ArrayList<>(projectedSyntaxMetadata);
            if (!sort && predicateSyntax != null) roots.add(predicateSyntax);
            hostWindow = new HostBatch.Window(roots, resolver, frame, () -> {
                if (sort && !streamingDistinct) return sorted.next();
                try { PersistentTree.Entry entry = scan.next(); return entry == null ? null : Encoding.row(entry, table.types()); }
                catch (IOException failure) { throw Engine.storageError(failure); }
            });
        }
        List<SqlValue> result = hostWindow.next(meter, CostMeter.saturatingAdd(remaining, skip), sort ? Costs.ROW_PRODUCED : Costs.STORAGE_ROW_READ);
        if (result != null && (!sort || streamingDistinct)) { meter.charge(Costs.STORAGE_ROW_READ); meter.guard(); }
        return result;
    }
    private List<SqlValue> nextInput(CostMeter meter) throws IOException {
        long before = meter.cost();
        try { PersistentTree.Entry entry = scan.next(); if (entry == null) return null;
            meter.charge(Costs.STORAGE_ROW_READ); meter.guard(); return Encoding.row(entry, table.types());
        } finally { metrics.add("Scan", before, meter); }
    }
    private void buildEagerDistinct(CostMeter meter) throws IOException {
        long before = meter.cost(), excluded = metrics.phase("Scan") + metrics.phase("Filter") + metrics.phase("Sort");
        try { buildEagerDistinctInternal(meter); }
        finally { if (distinct) metrics.addUnits("Distinct", meter.cost() - before - (metrics.phase("Scan") + metrics.phase("Filter") + metrics.phase("Sort") - excluded)); }
    }
    private void buildEagerDistinctInternal(CostMeter meter) throws IOException {
        // The eager lane finishes filtering before projection, and retains its source
        // until all distinct rows and membership entries have been admitted.
        try (RowSpool source = new RowSpool(table.types(), 0, null, meter.memory())) {
            List<SqlValue> values;
            boolean[] touched = resolver.touched();
            while ((values = nextInput(meter)) != null) {
                frame.values = values;
                if (!matches(meter)) continue;
                List<SqlValue> retained = new ArrayList<>(values.size());
                for (int c = 0; c < values.size(); c++) retained.add(touched[c] ? values.get(c) : SqlValue.nullValue(table.columns().get(c).type()));
                source.append(retained);
            }
            distinctRows = new RowSpool(types(), 0, null, meter.memory());
            seen = new DistinctState(null, 0, meter.memory());
            List<SqlType> sortedTypes = new ArrayList<>(types());
            for (ScalarPlan.Expression key : orderKeys) sortedTypes.add(CollationDerivation.sortType(key));
            sorter = new ExternalSorter(sortedTypes, 0, null, null, (left, right) -> compareKeys(left, right, projections.size()));
            try (RowSpool.Cursor cursor = source.cursor()) {
                while ((values = cursor.next()) != null) {
                    frame.values = values;
                    List<SqlValue> projected = new ArrayList<>();
                    for (ScalarPlan.Expression expression : projections) projected.add(expression.run(meter));
                    List<SqlValue> decorated = new ArrayList<>(projected);
                    for (ScalarPlan.Expression key : orderKeys) {
                        decorated.add(measuredSortKey(key, meter));
                    }
                    for (SqlValue value : projected) meter.charge(MemoryAccount.payload(value) * Costs.HASH_KEY);
                    meter.guard();
                    if (seen.add(projected)) { distinctRows.append(projected); sorter.append(decorated); }
                }
            }
            seen.close(); seen = null;
        }
        sorted = sorter.finish();
    }
    private int compareKeys(List<SqlValue> left, List<SqlValue> right, int firstKey) {
        for (int i = 0; i < orders.size(); i++) {
            SqlValue a = left.get(firstKey + i), b = right.get(firstKey + i); SqlParser.Order order = orders.get(i);
            boolean nullFirst = order.nullsFirst() == null ? order.descending() : order.nullsFirst();
            int comparison;
            if (a.isNull() || b.isNull()) comparison = a.isNull() == b.isNull() ? 0 : a.isNull() == nullFirst ? -1 : 1;
            else { comparison = ScalarPlan.compare(a, b, null); if (order.descending()) comparison = -comparison; }
            if (comparison != 0) return comparison;
        }
        return 0;
    }
    private void buildResidentSort(CostMeter meter) throws IOException {
        int width = table.columns().size();
        long capacity = remaining == 0 ? 0 : remaining == Long.MAX_VALUE ? Long.MAX_VALUE : CostMeter.saturatingAdd(remaining, skip);
        boolean collated = orderKeys.stream().anyMatch(key -> !key.collation().require().equals("C") && CollationDerivation.collatable(key.type()));
        residentSort = new ResidentSort(meter.memory(), capacity, (a, b) -> compareKeys(a, b, width), row -> {
            long bytes = collated ? 0 : MemoryAccount.row(row.subList(0, width));
            for (int i = 0; i < orderKeys.size(); i++) {
                ScalarPlan.Expression key = orderKeys.get(i); SqlValue value = row.get(width + i);
                if (!key.collation().require().equals("C") && CollationDerivation.collatable(key.type()) && !value.isNull()) bytes = MemoryAccount.add(bytes, MemoryAccount.value(value));
            }
            return bytes;
        });
        if (collated) { buildCollatedSort(meter, width, capacity); return; }
        List<SqlValue> values; boolean[] touched = resolver.touched();
        while ((values = nextInput(meter)) != null) {
            frame.values = values; if (!matches(meter)) continue;
            List<SqlValue> decorated = new ArrayList<>();
            for (int c = 0; c < width; c++) decorated.add(touched[c] ? values.get(c) : SqlValue.nullValue(table.columns().get(c).type()));
            for (ScalarPlan.Expression key : orderKeys) {
                decorated.add(measuredSortKey(key, meter));
            }
            residentSort.append(decorated);
        }
        sorted = residentSort.finish();
    }
    private void buildCollatedSort(CostMeter meter, int width, long capacity) throws IOException {
        List<List<SqlValue>> survivors = new ArrayList<>(); long sourceBytes = 0;
        try {
            List<SqlValue> values; boolean[] touched = resolver.touched();
            while ((values = nextInput(meter)) != null) {
                frame.values = values; if (!matches(meter)) continue;
                List<SqlValue> retained = new ArrayList<>();
                for (int c = 0; c < width; c++) retained.add(touched[c] ? values.get(c) : SqlValue.nullValue(table.columns().get(c).type()));
                long bytes = MemoryAccount.row(retained); meter.memory().reserve(bytes); sourceBytes = MemoryAccount.add(sourceBytes, bytes);
                survivors.add(retained);
            }
            // A finite selection transfers its input to the selector; only retained
            // decorations are live there. The selected rows are readmitted afterward.
            if (capacity != Long.MAX_VALUE) { meter.memory().release(sourceBytes); sourceBytes = 0; }
            for (List<SqlValue> survivor : survivors) {
                frame.values = survivor; List<SqlValue> decorated = new ArrayList<>(survivor);
                for (ScalarPlan.Expression key : orderKeys) {
                    decorated.add(measuredSortKey(key, meter));
                }
                residentSort.append(decorated);
            }
            meter.memory().release(sourceBytes); sourceBytes = 0;
            sorted = residentSort.finish(row -> MemoryAccount.row(row.subList(0, width)));
        } finally { meter.memory().release(sourceBytes); }
    }
    private void buildSort(CostMeter meter) throws IOException {
        long before = meter.cost(), excluded = metrics.phase("Scan") + metrics.phase("Filter") + metrics.phase("Sort");
        try { buildSortInternal(meter); }
        finally { if (distinct) metrics.addUnits("Distinct", meter.cost() - before - (metrics.phase("Scan") + metrics.phase("Filter") + metrics.phase("Sort") - excluded)); }
    }
    private void buildSortInternal(CostMeter meter) throws IOException {
        if (scratch == null && !distinct && distinctOn == null) { buildResidentSort(meter); return; }
        List<SqlType> types = new ArrayList<>(distinct ? types() : table.types()); for (ScalarPlan.Expression key : orderKeys) types.add(CollationDerivation.sortType(key));
        int firstKey = distinct ? projections.size() : table.columns().size();
        if (distinct) seen = new DistinctState(scratch, workMem, meter.memory());
        sorter = new ExternalSorter(types, workMem, scratch, meter.memory(), (left, right) -> {
            for (int i = 0; i < orders.size(); i++) {
                SqlValue a = left.get(firstKey + i), b = right.get(firstKey + i); SqlParser.Order order = orders.get(i);
                boolean nullFirst = order.nullsFirst() == null ? order.descending() : order.nullsFirst();
                int comparison;
                if (a.isNull() || b.isNull()) comparison = a.isNull() == b.isNull() ? 0 : a.isNull() == nullFirst ? -1 : 1;
                else { comparison = ScalarPlan.compare(a, b, null); if (order.descending()) comparison = -comparison; }
                if (comparison != 0) return comparison;
            }
            return 0;
        });
        List<SqlValue> values;
        while ((values = nextInput(meter)) != null) {
            frame.values = values; if (!matches(meter)) continue;
            List<SqlValue> withKeys = new ArrayList<>();
            if (distinct) {
                for (ScalarPlan.Expression expression : projections) withKeys.add(expression.run(meter));
            } else {
                boolean[] touched = resolver.touched();
                for (int c = 0; c < values.size(); c++) withKeys.add(touched[c] ? values.get(c) : SqlValue.nullValue(table.columns().get(c).type()));
            }
            for (int i = 0; i < orderKeys.size(); i++) {
                withKeys.add(measuredSortKey(orderKeys.get(i), meter));
            }
            sorter.append(withKeys);
        }
        sorted = sorter.finish();
        if (distinct) {
            distinctRows = new RowSpool(types(), 0, null, meter.memory());
            List<SqlValue> input;
            while ((input = sorted.next()) != null) {
                List<SqlValue> projected = input.subList(0, projections.size());
                for (SqlValue value : projected) meter.charge(MemoryAccount.payload(value) * Costs.HASH_KEY);
                meter.guard();
                if (seen.add(projected)) distinctRows.append(projected);
            }
            sorted.close(); sorter.close(); sorter = null;
            sorted = distinctRows.cursor();
        }
    }
    private SqlValue measuredSortKey(ScalarPlan.Expression key, CostMeter meter) {
        long before = meter.cost();
        try { SqlValue value = key.run(meter);
            if (key.collation().require().equals("C") || !CollationDerivation.collatable(value.type())) { meter.charge(MemoryAccount.payload(value) * Costs.SORT_KEY); meter.guard(); }
            return CollationDerivation.sortValue(key, value, meter);
        } finally { metrics.add("Sort", before, meter); }
    }
    private boolean matches(CostMeter meter) { long before = meter.cost(); try { if (where == null) return true; SqlValue value = where.run(meter); return !value.isNull() && value.asBoolean(); } finally { metrics.add("Filter", before, meter); } }
    @Override public void close() { if (gatheredAccount != null) { gatheredAccount.release(gatheredBytes); gatheredBytes = 0; gathered.clear(); } if (hostWindow != null) hostWindow.close(); if (scan != null) scan.close(); if (sorted != null) sorted.close(); if (sorter != null) sorter.close(); if (residentSort != null) residentSort.close(); if (seen != null) seen.close(); if (distinctRows != null) distinctRows.close(); }
    private static long bound(ScalarPlan.Expression expression, CostMeter meter, long fallback, String state) {
        if (expression == null) return fallback; SqlValue value = expression.run(meter); if (value.isNull()) return fallback;
        long number = value.asInteger(); if (number < 0) throw new SqlException(state, "negative row bound"); return number;
    }
    static List<ScalarPlan.Expression> point(SqlParser.Expr predicate, Engine.Table table, List<String> names, String alias, ScalarPlan.Resolver constants) {
        if (table.primaryKey().length == 0) return null;
        List<SqlParser.Expr> terms = new ArrayList<>(); conjuncts(predicate, terms);
        List<ScalarPlan.Expression> key = new ArrayList<>();
        for (int column : table.primaryKey()) {
            ScalarPlan.Expression found = null;
            for (SqlParser.Expr term : terms) {
                if (!(term instanceof SqlParser.Binary binary) || !binary.op().equals("=")) continue;
                SqlParser.Expr other = null;
                if (column(binary.left(), names.get(column), alias)) other = binary.right();
                else if (column(binary.right(), names.get(column), alias)) other = binary.left();
                ScalarPlan.Expression expression = TableScan.constant(constants, other, table.columns().get(column).type());
                if (expression == null) continue;
                if (TableScan.keyCompatible(expression.type(), table.columns().get(column).type())) { found = expression; break; }
            }
            if (found == null) return null; key.add(found);
        }
        return key;
    }
    private static boolean column(SqlParser.Expr expression, String name, String alias) {
        return expression instanceof SqlParser.Name n && n.parts().getLast().equals(name) && (n.parts().size() == 1 || n.parts().size() == 2 && n.parts().getFirst().equals(alias));
    }
    private static void conjuncts(SqlParser.Expr expression, List<SqlParser.Expr> terms) {
        if (expression instanceof SqlParser.Binary b && b.op().equals("and")) { conjuncts(b.left(), terms); conjuncts(b.right(), terms); }
        else if (expression != null) terms.add(expression);
    }
}
