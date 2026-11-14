package org.jed;

import java.io.IOException;
import java.nio.file.Path;
import java.util.*;
import org.jed.host.PageConnection;
import org.jed.host.PageProtocol;
import org.jed.storage.PageFormat;

/** Models exclusive crash-clean locks and endpoint publication, never database policy. */
final class SimulationRoleRegistry {
    private record Owner(int process, byte[] incarnation, Database database, PageService service) { }
    private final Map<String,Owner> owners = new TreeMap<>();
    private final SimulationNetwork network;
    private long generation;
    SimulationRoleRegistry(SimulationNetwork network) { this.network = network; }
    synchronized Integer owner(String path) { Owner owner = owners.get(path); return owner == null ? null : owner.process; }
    synchronized Database database(String path) { Owner owner = owners.get(path); return owner == null ? null : owner.database; }
    void kill(int process) throws IOException {
        network.kill(process);
        List<Database> doomed;
        synchronized (this) { doomed = owners.values().stream().filter(owner -> owner.process == process && owner.database != null).map(Owner::database).toList(); owners.entrySet().removeIf(entry -> entry.getValue().process == process); }
        for (Database database : doomed) database.abortInjectedHost();
        network.settle();
    }
    FileRoleHost host(int process) {
        return new FileRoleHost() {
            public PageConnection elect(Path path, boolean readOnly) throws IOException {
                String name = path.toString(); Owner owner;
                synchronized (SimulationRoleRegistry.this) {
                    owner = owners.get(name);
                    if (owner == null) { byte[] incarnation = new byte[16]; PageFormat.put64(incarnation, 8, ++generation); owners.put(name, new Owner(process, incarnation, null, null)); return null; }
                    if (owner.process == process || owner.service == null) throw new IllegalStateException("duplicate or unpublished simulated ownership");
                }
                var link = network.connect(process, owner.process, name); link.serve(owner.service); var streams = link.client.connection();
                try { return new PageConnection(streams.input(), streams.output(), streams, new PageProtocol.Hello(1, 1, PageFormat.VERSION, readOnly ? 1 : 0, "java-simulation", PageProtocol.WireCredential.none()), notice -> { }); }
                catch (IOException | RuntimeException | Error failure) { streams.close(); throw failure; }
            }
            public byte[] incarnation(Path path) { synchronized (SimulationRoleRegistry.this) { return owners.get(path.toString()).incarnation.clone(); } }
            public void publish(Path path, Database database, PageService service) {
                synchronized (SimulationRoleRegistry.this) { Owner owner = owners.get(path.toString()); if (owner == null || owner.process != process) throw new IllegalStateException("service without exclusive ownership"); owners.put(path.toString(), new Owner(process, owner.incarnation, database, service)); }
            }
            public void release(Path path) { synchronized (SimulationRoleRegistry.this) { Owner owner = owners.get(path.toString()); if (owner != null && owner.process == process) owners.remove(path.toString()); } }
        };
    }
}
