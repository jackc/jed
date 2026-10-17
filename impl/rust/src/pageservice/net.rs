//! The page service's stream seam (spec/pageservice/README.md §2, §5.2): the connection and listener
//! the owner and its clients speak over. Production uses Unix-domain sockets; the deterministic
//! simulator (spec/design/simulation-testing.md §14 S8) substitutes in-memory streams through the lock
//! host ([`crate::coordinator::LockHost`]). Every frame is written with one `write_all` of the whole
//! encoded frame, so a stream that accepts each write whole counts frames by write calls.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

/// One end of a page-service connection. Clones share the end: closing any of them closes it, and
/// the peer reads end of stream once what was written before drains.
pub(crate) trait PsStream: Read + Write + Send + Sync {
    /// Another handle on the same end (a reader thread's, a sender's, a control handle).
    fn clone_stream(&self) -> std::io::Result<Box<dyn PsStream>>;
    /// Close this end in both directions; a reader blocked on any handle of it wakes.
    fn close_both(&self);
}

/// An owner's endpoint.
pub(crate) trait PsListener: Send + Sync {
    /// The next connection. `Ok(None)` once the listener is closed.
    fn accept(&self) -> std::io::Result<Option<Box<dyn PsStream>>>;
    /// Stop accepting: a blocked [`accept`](Self::accept) wakes.
    fn close(&self);
}

impl PsStream for UnixStream {
    fn clone_stream(&self) -> std::io::Result<Box<dyn PsStream>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn close_both(&self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}

/// The production endpoint: a bound Unix-domain socket.
pub(crate) struct UnixEndpoint {
    listener: UnixListener,
    path: PathBuf,
}

impl UnixEndpoint {
    pub(crate) fn bind(path: PathBuf) -> std::io::Result<UnixEndpoint> {
        let listener = UnixListener::bind(&path)?;
        Ok(UnixEndpoint { listener, path })
    }
}

impl PsListener for UnixEndpoint {
    fn accept(&self) -> std::io::Result<Option<Box<dyn PsStream>>> {
        let (stream, _) = self.listener.accept()?;
        Ok(Some(Box::new(stream)))
    }

    /// Wake the blocking accept by connecting to the endpoint; the accept loop then sees the owner
    /// closed and returns.
    fn close(&self) {
        let _ = UnixStream::connect(&self.path);
    }
}

/// Dial a production endpoint.
pub(crate) fn dial_unix(endpoint: &str) -> std::io::Result<Box<dyn PsStream>> {
    Ok(Box::new(UnixStream::connect(endpoint)?))
}
