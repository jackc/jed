package org.jed;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import org.jed.storage.BlockStore;
import org.jed.storage.FileBlockStore;

/** Disposable byte-device factory; scratch codecs and spill policy remain in the core. */
@FunctionalInterface
interface ScratchHost {
    BlockStore create(Path directory, boolean unlink) throws IOException;
    ScratchHost NATIVE = (directory, unlink) -> {
        Path path = SpillRows.create(directory);
        BlockStore device = null;
        try {
            device = FileBlockStore.openScratch(path);
            if (unlink) { Files.delete(path); return device; }
            BlockStore opened = device;
            return new BlockStore() {
                public byte[] readAt(long offset, int count) throws IOException { return opened.readAt(offset, count); }
                public void writeAt(long offset, byte[] bytes) throws IOException { opened.writeAt(offset, bytes); }
                public long size() throws IOException { return opened.size(); }
                public void setSize(long length) throws IOException { opened.setSize(length); }
                public void sync() throws IOException { opened.sync(); }
                public void close() throws IOException {
                    IOException failure = null;
                    try { opened.close(); } catch (IOException error) { failure = error; }
                    try { Files.deleteIfExists(path); } catch (IOException error) { if (failure == null) failure = error; else failure.addSuppressed(error); }
                    if (failure != null) throw failure;
                }
            };
        } catch (IOException | RuntimeException | Error failure) {
            if (device != null) try { device.close(); } catch (IOException cleanup) { failure.addSuppressed(cleanup); }
            try { Files.deleteIfExists(path); } catch (IOException cleanup) { failure.addSuppressed(cleanup); }
            throw failure;
        }
    };
}
