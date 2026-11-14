package org.jed;

import java.io.*;
import java.util.*;

/** Rendezvous duplex streams with deterministic frame disconnects and quiescence. */
final class SimulationNetwork {
    private final List<Link> links = new ArrayList<>();
    private int frames, disconnectAt;
    boolean disconnected;
    synchronized void begin(int nth) { frames = 0; disconnectAt = nth; disconnected = false; }
    synchronized Link connect(int client, int owner, String path) { Link link = new Link(client, owner, path); links.add(link); return link; }
    synchronized void disconnect(int client, String path) { for (Link link : links) if (link.clientId == client && link.path.equals(path)) link.end(); }
    synchronized void kill(int process) { for (Link link : links) if (link.clientId == process || link.ownerId == process) link.end(); }
    synchronized void settle() throws IOException {
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (links.stream().anyMatch(link -> !link.settled())) {
            if (System.nanoTime() >= deadline) throw new IOException("simulation transport did not reach quiescence");
            waitForChange();
        }
    }
    private void waitForChange() throws IOException { try { wait(25); } catch (InterruptedException failure) { Thread.currentThread().interrupt(); throw new IOException("simulation transport interrupted", failure); } }
    final class Link {
        final int clientId, ownerId; final String path;
        final Endpoint client = new Endpoint(this), server = new Endpoint(this);
        boolean ended, finished;
        Link(int clientId, int ownerId, String path) { this.clientId = clientId; this.ownerId = ownerId; this.path = path; client.peer = server; server.peer = client; }
        void serve(PageService service) { Thread.ofVirtual().name("jed-simulation-owner").start(() -> { try { service.serve(server.connection()); } finally { synchronized (SimulationNetwork.this) { finished = true; SimulationNetwork.this.notifyAll(); } } }); }
        void end() { ended = true; SimulationNetwork.this.notifyAll(); }
        boolean settled() { return ended ? finished && client.readEnded : client.waiting && server.waiting; }
    }
    private final class Packet { final byte[] bytes; boolean consumed; Packet(byte[] bytes) { this.bytes = bytes; } }
    final class Endpoint {
        final Link link; Endpoint peer;
        final ArrayDeque<Packet> queued = new ArrayDeque<>();
        boolean waiting, readEnded; int offset;
        Endpoint(Link link) { this.link = link; }
        PageTransport.Connection connection() {
            InputStream input = new InputStream() {
                public int read() throws IOException { byte[] one = new byte[1]; return read(one, 0, 1) < 0 ? -1 : one[0] & 255; }
                public int read(byte[] bytes, int start, int count) throws IOException {
                    if (count == 0) return 0;
                    synchronized (SimulationNetwork.this) {
                        for (;;) {
                            if (link.ended) { readEnded = true; waiting = false; SimulationNetwork.this.notifyAll(); return -1; }
                            Packet packet = queued.peekFirst();
                            if (packet != null) {
                                waiting = false; int copied = Math.min(count, packet.bytes.length - offset); System.arraycopy(packet.bytes, offset, bytes, start, copied); offset += copied;
                                if (offset == packet.bytes.length) { queued.removeFirst(); offset = 0; packet.consumed = true; }
                                SimulationNetwork.this.notifyAll(); return copied;
                            }
                            waiting = true; SimulationNetwork.this.notifyAll(); waitForChange();
                        }
                    }
                }
            };
            OutputStream output = new OutputStream() {
                public void write(int value) throws IOException { write(new byte[] {(byte)value}); }
                public void write(byte[] bytes, int start, int count) throws IOException {
                    if (count == 0) return;
                    synchronized (SimulationNetwork.this) {
                        if (link.ended) throw new IOException("simulated connection lost");
                        if (++frames == disconnectAt) { disconnected = true; link.end(); throw new IOException("simulated frame disconnect"); }
                        Packet packet = new Packet(Arrays.copyOfRange(bytes, start, start + count)); peer.queued.addLast(packet); peer.waiting = false; SimulationNetwork.this.notifyAll();
                        while (!packet.consumed && !link.ended) waitForChange();
                        if (!packet.consumed) throw new IOException("simulated connection lost");
                    }
                }
            };
            return new PageTransport.Connection(input, output, () -> { synchronized (SimulationNetwork.this) { readEnded = true; link.end(); } });
        }
    }
}
