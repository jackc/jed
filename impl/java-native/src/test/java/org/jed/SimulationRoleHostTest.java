package org.jed;

import java.nio.file.Path;

/** Injected host exercises the production owner, client, disconnect and promotion paths. */
public final class SimulationRoleHostTest {
    public static void main(String[] args) throws Exception {
        var fs = new SimulationFileSystem(1, 512, 64); var network = new SimulationNetwork(); var roles = new SimulationRoleRegistry(network);
        var base = FileOptions.builder().storageHost(fs).serve(false).idleFlush(false).pageSize(512).build();
        try (var created = Database.create(Path.of("sim.jed"), base)) { }
        var options = base.toBuilder().serve(true).shutdownGraceMillis(0).idleSessionMillis(0).idleTransactionMillis(0).build();
        try (var owner = Database.open(Path.of("sim.jed"), options.toBuilder().roleHost(roles.host(0)).build());
             var client = Database.open(Path.of("sim.jed"), options.toBuilder().roleHost(roles.host(1)).build())) {
            client.execute("CREATE TABLE t (id i32 PRIMARY KEY)"); client.execute("INSERT INTO t VALUES (1)"); network.settle();
            try (var read = owner.query("SELECT id FROM t")) { check(read.next().get(0).asInteger() == 1, "client publication"); }
            try (var empty = client.session()) { empty.execute("BEGIN"); network.begin(1); empty.execute("COMMIT"); }
            network.settle(); check(network.disconnected, "empty commit abort frame lost"); network.begin(0);
            network.disconnect(1, "sim.jed"); network.settle(); client.execute("INSERT INTO t VALUES (2)"); network.settle();
            roles.kill(0); client.execute("INSERT INTO t VALUES (3)"); network.settle();
            check(roles.owner("sim.jed") == 1, "disconnected client promotes under exclusive host grant");
            try (var rows = client.query("SELECT count(*) FROM t")) { check(rows.next().get(0).asInteger() == 3, "promotion preserved published rows"); }
        }
        network.settle(); System.out.println("SimulationRoleHostTest passed (production protocol, reconnect and crash-clean promotion)");
    }
    private static void check(boolean condition, String message) { if (!condition) throw new AssertionError(message); }
}
