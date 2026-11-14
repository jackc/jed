package org.jed;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import org.jed.host.LinuxNative;
import org.jed.storage.BlockStore;
import org.jed.storage.FileBlockStore;

/** File lifecycle seam shared by native storage and deterministic host fault simulation. */
interface FileHost {
    BlockStore open(Path path, boolean writable) throws IOException;
    BlockStore create(Path path) throws IOException;
    /** Replace an unpublished temporary image, including a leftover from an interrupted rewrite. */
    BlockStore rewrite(Path path) throws IOException;
    boolean exists(Path path) throws IOException;
    void delete(Path path) throws IOException;
    void rename(Path source, Path target, boolean replace) throws IOException;
    void syncDirectory(Path path) throws IOException;
    void permissions(Path source, Path target) throws IOException;
    FileHost NATIVE = new FileHost() {
        public BlockStore open(Path path, boolean writable) throws IOException { return FileBlockStore.open(path, writable); }
        public BlockStore create(Path path) throws IOException { return FileBlockStore.createNew(path); }
        public BlockStore rewrite(Path path) throws IOException { return FileBlockStore.createOrTruncate(path); }
        public boolean exists(Path path) { return Files.exists(path, java.nio.file.LinkOption.NOFOLLOW_LINKS); }
        public void delete(Path path) throws IOException { Files.deleteIfExists(path); }
        public void rename(Path source, Path target, boolean replace) throws IOException {
            if (replace) Files.move(source, target, java.nio.file.StandardCopyOption.ATOMIC_MOVE, java.nio.file.StandardCopyOption.REPLACE_EXISTING);
            else LinuxNative.renameNoReplace(source, target);
        }
        public void syncDirectory(Path path) throws IOException { LinuxNative.syncDirectory(path); }
        public void permissions(Path source, Path target) throws IOException { Files.setPosixFilePermissions(target, Files.getPosixFilePermissions(source)); }
    };
}
