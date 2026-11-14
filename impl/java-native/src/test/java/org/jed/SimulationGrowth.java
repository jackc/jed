package org.jed;

import java.io.IOException;
import java.util.*;

/** Shared main/attachment reclamation oracle from simulation-testing §8.3. */
final class SimulationGrowth {
    private final SimulationFileSystem fs;
    private final String path;
    private long allowance;
    private boolean previouslyReclaimed;
    SimulationGrowth(SimulationFileSystem fs, String path) { this.fs = fs; this.path = path; }
    FileDatabase.Inspection inspect(Database database) throws IOException {
        boolean old = fs.oracleReads; fs.oracleReads = true;
        try { return database.inspectionForTesting(); } finally { fs.oracleReads = old; }
    }
    void reset(Database database) throws IOException { allowance = Math.max(0, inspect(database).leaked()); previouslyReclaimed = false; }
    void committed(Database database, FileDatabase.Inspection before, boolean quiet) throws IOException {
        var after = inspect(database); long first = after.meta().layout().firstBody(); int pageSize = after.meta().pageSize();
        Set<Long> written = new TreeSet<>(); for (long page : fs.written.getOrDefault(path, Set.of())) if (page >= first) written.add(page);
        boolean reclaim = quiet && !before.pending();
        if (quiet) {
            if (reclaim) {
                if (before.reclaimDue()) allowance = 0;
                long unlisted = after.meta().pageCount() - first - after.meta().livePages() - after.free().length;
                require(unlisted >= 0 && unlisted <= written.size() + allowance, "quiet commit leaves new unreachable pages unlisted");
            }
            Set<Long> reusable = new HashSet<>(); for (long page : before.reusable()) reusable.add(page);
            if (!before.pending() && previouslyReclaimed) require(after.meta().pageCount() - before.meta().pageCount() <= Math.max(0, written.size() - reusable.size()), "commit grew without using reclaimed free pages");
            long tail = before.meta().pageCount(); if (!before.pending()) while (tail > first && reusable.contains(tail - 1)) tail--;
            long high = written.stream().mapToLong(Long::longValue).max().orElse(-1) + 1;
            require(after.meta().pageCount() == Math.max(tail, high), "commit violates canonical trailing high-water");
            if (!after.pending() && after.meta().pageCount() < before.meta().pageCount()) {
                long slack = Math.clamp(after.meta().pageCount(), Math.max(1, 16384 / pageSize), Math.max(1, 1048576 / pageSize));
                require(fs.image(path).length <= (after.meta().pageCount() + slack) * pageSize, "truncation retains excess allocation slack");
            }
        }
        previouslyReclaimed = reclaim;
    }
    private void require(boolean condition, String message) { SimulationStorageChecks.require(condition, path + ": " + message); }
}
