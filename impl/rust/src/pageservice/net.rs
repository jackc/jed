//! The page service's stream seam (spec/pageservice/README.md §2, §5.2): the connection and listener
//! the owner and its clients speak over. Production uses Unix-domain sockets; the deterministic
//! simulator (spec/design/simulation-testing.md §14 S8) substitutes in-memory streams through the lock
//! host ([`crate::coordinator::LockHost`]). Every frame is written with one `write_all` of the whole
//! encoded frame, so a stream that accepts each write whole counts frames by write calls.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

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

/// A page-service connection over a pair of byte pipes: a child process's stdio on the client side
/// (the SSH transport, spec/pageservice/README.md §5.3), this process's stdin/stdout on the
/// `jed pages` side. A pump thread moves the input pipe into a buffer, so [`close_both`] wakes a
/// blocked reader at once even though a pipe read cannot be interrupted. Each `write` writes and
/// flushes the whole buffer, so a frame written with one `write_all` leaves the process whole.
///
/// [`close_both`]: PsStream::close_both
pub(crate) struct PipeStream {
    inner: Arc<PipeInner>,
}

struct PipeInner {
    input: Mutex<PipeInput>,
    /// Signalled when input arrives, the input pipe ends, the stream closes, or a buffer drains.
    changed: Condvar,
    /// The output pipe; `None` once closed, which closes the pipe (a child sees end of input).
    output: Mutex<Option<Box<dyn Write + Send>>>,
    /// Run once at close (the client ends its transport process).
    on_close: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

struct PipeInput {
    buf: VecDeque<u8>,
    /// The input pipe ended (or failed).
    eof: bool,
    closed: bool,
}

/// How much input the pump buffers ahead of the reader before it waits.
const PIPE_BUFFER_LIMIT: usize = 8 << 20;

impl PipeStream {
    /// A stream reading `input` (through a pump thread) and writing `output`. `prefix` is input
    /// already read from the pipe, served first; `on_close` runs once when the stream closes.
    pub(crate) fn new(
        prefix: Vec<u8>,
        input: Box<dyn Read + Send>,
        output: Box<dyn Write + Send>,
        on_close: Option<Box<dyn FnOnce() + Send>>,
    ) -> PipeStream {
        let inner = Arc::new(PipeInner {
            input: Mutex::new(PipeInput {
                buf: prefix.into(),
                eof: false,
                closed: false,
            }),
            changed: Condvar::new(),
            output: Mutex::new(Some(output)),
            on_close: Mutex::new(on_close),
        });
        let pump = Arc::clone(&inner);
        std::thread::spawn(move || pump.pump(input));
        PipeStream { inner }
    }

    /// Another handle on the same stream.
    pub(crate) fn clone_pipe(&self) -> PipeStream {
        PipeStream {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Wait until the stream is closed (by any handle).
    pub(crate) fn wait_closed(&self) {
        let mut input = self.inner.input.lock().expect("pipe lock poisoned");
        while !input.closed {
            input = self.inner.changed.wait(input).expect("pipe lock poisoned");
        }
    }
}

impl PipeInner {
    fn pump(&self, mut input: Box<dyn Read + Send>) {
        let mut chunk = vec![0u8; 64 << 10];
        loop {
            let n = match input.read(&mut chunk) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => 0,
            };
            let mut state = self.input.lock().expect("pipe lock poisoned");
            if n == 0 {
                state.eof = true;
                self.changed.notify_all();
                return;
            }
            while state.buf.len() >= PIPE_BUFFER_LIMIT && !state.closed {
                state = self.changed.wait(state).expect("pipe lock poisoned");
            }
            if state.closed {
                return;
            }
            state.buf.extend(&chunk[..n]);
            self.changed.notify_all();
        }
    }

    fn close(&self) {
        {
            let mut state = self.input.lock().expect("pipe lock poisoned");
            state.closed = true;
            state.buf.clear();
            self.changed.notify_all();
        }
        drop(self.output.lock().expect("pipe lock poisoned").take());
        let on_close = self.on_close.lock().expect("pipe lock poisoned").take();
        if let Some(on_close) = on_close {
            on_close();
        }
    }
}

impl Read for PipeStream {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut state = self.inner.input.lock().expect("pipe lock poisoned");
        loop {
            if state.closed {
                return Ok(0);
            }
            if !state.buf.is_empty() {
                let n = out.len().min(state.buf.len());
                for (slot, byte) in out.iter_mut().zip(state.buf.drain(..n)) {
                    *slot = byte;
                }
                self.inner.changed.notify_all();
                return Ok(n);
            }
            if state.eof {
                return Ok(0);
            }
            state = self.inner.changed.wait(state).expect("pipe lock poisoned");
        }
    }
}

impl Write for PipeStream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut output = self.inner.output.lock().expect("pipe lock poisoned");
        let Some(out) = output.as_mut() else {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        };
        out.write_all(bytes)?;
        out.flush()?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl PsStream for PipeStream {
    fn clone_stream(&self) -> std::io::Result<Box<dyn PsStream>> {
        Ok(Box::new(PipeStream {
            inner: Arc::clone(&self.inner),
        }))
    }

    fn close_both(&self) {
        self.inner.close();
    }
}
