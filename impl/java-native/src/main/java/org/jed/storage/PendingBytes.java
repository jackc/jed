package org.jed.storage;

import java.util.Arrays;

/** Immutable per-key accounting for record versions retained by a transaction. */
public final class PendingBytes {
    private static final ThreadLocal<Boolean> TRACK = ThreadLocal.withInitial(() -> false);
    public static final class Scope implements AutoCloseable {
        private final boolean previous;
        private Scope(boolean enabled) { previous = TRACK.get(); TRACK.set(enabled); }
        @Override public void close() { TRACK.set(previous); }
    }
    public static Scope tracking(boolean enabled) { return new Scope(enabled); }
    private record Node(byte[] key, long bytes, Node left, Node right, int height) {
        Node(byte[] key, long bytes, Node left, Node right) { this(key, bytes, left, right, 1 + Math.max(height(left), height(right))); }
        private static int height(Node node) { return node == null ? 0 : node.height; }
    }
    static final PendingBytes EMPTY = new PendingBytes(0, null);
    final long bytes;
    private final Node root;
    private PendingBytes(long bytes, Node root) { this.bytes = bytes; this.root = root; }
    PendingBytes forget() { return root == null ? this : new PendingBytes(bytes, null); }
    PendingBytes write(byte[] key, long size) {
        long old = find(root, key), total = Math.addExact(bytes - old, size);
        return new PendingBytes(total, update(root, key, TRACK.get() ? size : 0));
    }
    PendingBytes remove(byte[] key) { long old = find(root, key); return old == 0 ? this : new PendingBytes(bytes - old, update(root, key, 0)); }
    private static long find(Node node, byte[] key) {
        while (node != null) { int order = Arrays.compareUnsigned(key, node.key); if (order == 0) return node.bytes; node = order < 0 ? node.left : node.right; }
        return 0;
    }
    private static Node update(Node node, byte[] key, long bytes) {
        if (node == null) return bytes == 0 ? null : new Node(key.clone(), bytes, null, null);
        int order = Arrays.compareUnsigned(key, node.key);
        if (order < 0) return balance(new Node(node.key, node.bytes, update(node.left, key, bytes), node.right));
        if (order > 0) return balance(new Node(node.key, node.bytes, node.left, update(node.right, key, bytes)));
        if (bytes != 0) return new Node(node.key, bytes, node.left, node.right);
        if (node.left == null) return node.right; if (node.right == null) return node.left;
        Node next = node.right; while (next.left != null) next = next.left;
        return balance(new Node(next.key, next.bytes, node.left, update(node.right, next.key, 0)));
    }
    private static Node balance(Node node) {
        int diff = Node.height(node.left) - Node.height(node.right);
        if (diff > 1) {
            Node left = node.left;
            if (Node.height(left.left) < Node.height(left.right)) left = rotateLeft(left);
            return rotateRight(new Node(node.key, node.bytes, left, node.right));
        }
        if (diff < -1) {
            Node right = node.right;
            if (Node.height(right.right) < Node.height(right.left)) right = rotateRight(right);
            return rotateLeft(new Node(node.key, node.bytes, node.left, right));
        }
        return node;
    }
    private static Node rotateLeft(Node node) { Node right = node.right; return new Node(right.key, right.bytes, new Node(node.key, node.bytes, node.left, right.left), right.right); }
    private static Node rotateRight(Node node) { Node left = node.left; return new Node(left.key, left.bytes, left.left, new Node(node.key, node.bytes, left.right, node.right)); }
}
