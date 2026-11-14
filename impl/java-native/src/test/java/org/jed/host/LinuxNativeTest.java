package org.jed.host;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Comparator;

public final class LinuxNativeTest {
    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("busy")) {
            try (var child = LinuxNative.openLockFile(Path.of(args[1]))) {
                check(!child.tryExclusive(), "child acquired parent's lock");
            }
            return;
        }
        Path directory = Files.createTempDirectory("jed-java-native-");
        try {
            Path lock = directory.resolve("presence");
            try (var first = LinuxNative.openLockFile(lock);
                 var second = LinuxNative.openLockFile(lock)) {
                check(first.tryExclusive(), "initial exclusive lock");
                check(!second.tryShared(), "shared acquired over exclusive");
                check(!second.tryExclusive(), "exclusive acquired over exclusive");
                Process child = new ProcessBuilder(
                    Path.of(System.getProperty("java.home"), "bin", "java").toString(),
                    "--enable-native-access=ALL-UNNAMED", "-cp", System.getProperty("java.class.path"),
                    LinuxNativeTest.class.getName(), "busy", lock.toString()).inheritIO().start();
                check(child.waitFor() == 0, "child lock check");
                first.unlock();
                check(first.tryShared() && second.tryShared(), "shared holders coexist");
                first.unlock();
                second.unlock();
                check(second.tryExclusive(), "exclusive after shared release");
            }
            Path symlink = directory.resolve("link");
            Files.createSymbolicLink(symlink, lock);
            try (var unexpected = LinuxNative.openLockFile(symlink)) {
                throw new AssertionError("lock followed symlink");
            } catch (LinuxNative.NativeIOException expected) {
                check(expected.errno() == 40, "symlink errno");
            }
            Path source = directory.resolve("temporary");
            Path target = directory.resolve("published");
            Files.writeString(source, "first");
            LinuxNative.renameNoReplace(source, target);
            Files.writeString(source, "second");
            try {
                LinuxNative.renameNoReplace(source, target);
                throw new AssertionError("rename clobbered target");
            } catch (LinuxNative.NativeIOException expected) {
                check(expected.errno() == 17, "duplicate target errno");
            }
            check(Files.readString(target).equals("first"), "target preserved");
            LinuxNative.syncDirectory(directory);
            System.out.println("LinuxNativeTest passed");
        } finally {
            try (var files = Files.walk(directory)) {
                for (Path path : files.sorted(Comparator.reverseOrder()).toList()) Files.delete(path);
            }
        }
    }

    private static void check(boolean condition, String message) {
        if (!condition) throw new AssertionError(message);
    }
}
