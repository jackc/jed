package org.jed;

import java.io.IOException;
import java.nio.file.Path;
import org.jed.host.PageConnection;

/** Injected exclusive-ownership and endpoint host; database policy remains in the core. */
interface FileRoleHost {
    /** Null grants exclusive ownership; otherwise returns a connected page client. */
    PageConnection elect(Path path, boolean readOnly) throws IOException;
    byte[] incarnation(Path path);
    void publish(Path path, Database database, PageService service) throws IOException;
    void release(Path path);
}
