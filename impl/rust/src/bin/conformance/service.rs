//! The corpus through a page client (spec/pageservice/README.md §7): in `service` mode another
//! core's process actor owns the database while each query record runs here as a page client of it,
//! so rows, errors, plans, and costs are checked across the page service and across cores.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

/// The other core's process actor, started once with `owner-idle` and told to open and close each
/// corpus file's database as its owner.
pub struct OwnerActor {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Whether the actor currently owns the running file's database.
    owns: bool,
}

/// The owner actor in service mode; `None` in the memory and disk modes.
static ACTOR: Mutex<Option<OwnerActor>> = Mutex::new(None);

/// The query records a page client answered, and those it refused as writes (`25006`) that the
/// owner then ran.
pub static CLIENT_QUERIES: AtomicU32 = AtomicU32::new(0);
pub static OWNER_RETRIES: AtomicU32 = AtomicU32::new(0);

/// Start the owner actor of `core` (`rust`, `go`, or `node`) for the repository at `root`.
pub fn start(core: &str, root: &Path) -> Result<(), String> {
    let mut argv: Vec<PathBuf> = match core {
        "rust" => vec![root.join("impl/rust/target/release/process_actor")],
        "go" => vec![root.join("impl/go/process_actor")],
        "node" => vec![
            PathBuf::from("node"),
            root.join("impl/ts/src/bin/process_actor.ts"),
        ],
        other => {
            return Err(format!(
                "JED_SERVICE_OWNER must be rust, go, or node; got {other:?}"
            ));
        }
    };
    argv.push(PathBuf::from("owner-idle"));
    argv.push(root.to_path_buf());
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("start {core} owner actor: {e}"))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .map_err(|e| format!("{core} owner actor: {e}"))?;
    if line != "READY\n" {
        return Err(format!("{core} owner actor did not start: {line:?}"));
    }
    *ACTOR.lock().expect("actor lock poisoned") = Some(OwnerActor {
        child,
        stdin,
        stdout,
        owns: false,
    });
    Ok(())
}

/// Whether service mode is running.
pub fn active() -> bool {
    ACTOR.lock().expect("actor lock poisoned").is_some()
}

impl OwnerActor {
    fn command(&mut self, name: &str, argument: &str) -> Result<(), String> {
        let line = if argument.is_empty() {
            format!("{name}\n")
        } else {
            format!("{name}\t{argument}\n")
        };
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("owner actor: {e}"))?;
        let mut reply = String::new();
        self.stdout
            .read_line(&mut reply)
            .map_err(|e| format!("owner actor: {e}"))?;
        if reply.starts_with("OK") {
            return Ok(());
        }
        let mut fields = reply.trim_end().splitn(3, '\t');
        let status = fields.next().unwrap_or("");
        let code = fields.next().unwrap_or("");
        let detail = fields.next().unwrap_or("");
        let message: Vec<u8> = detail
            .as_bytes()
            .chunks_exact(2)
            .filter_map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
            .collect();
        Err(format!(
            "owner actor {name} failed: {status} {code} {}",
            String::from_utf8_lossy(&message)
        ))
    }
}

/// Before a reopen: make the actor own the database at `path` when the next handle is a page
/// client, and release it when the next handle is the owner (README.md §7).
pub fn prepare_reopen(path: &Path, client: bool) -> Result<(), String> {
    let mut guard = ACTOR.lock().expect("actor lock poisoned");
    let Some(actor) = guard.as_mut() else {
        return Ok(());
    };
    if client == actor.owns {
        return Ok(());
    }
    if client {
        let hex: String = path
            .to_string_lossy()
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect();
        actor.command("OWNER_OPEN", &hex)?;
    } else {
        actor.command("OWNER_CLOSE", "")?;
    }
    actor.owns = client;
    Ok(())
}

/// At the end of a file: the actor closes the database it owns, so the file can be removed.
pub fn release() {
    if let Some(actor) = ACTOR.lock().expect("actor lock poisoned").as_mut()
        && actor.owns
    {
        let _ = actor.command("OWNER_CLOSE", "");
        actor.owns = false;
    }
}

/// Stop the actor at the end of the run.
pub fn stop() {
    if let Some(mut actor) = ACTOR.lock().expect("actor lock poisoned").take() {
        let _ = actor.command("CLOSE", "");
        let _ = actor.child.wait();
    }
}

pub fn count_client_query() {
    CLIENT_QUERIES.fetch_add(1, Ordering::Relaxed);
}

pub fn count_owner_retry() {
    OWNER_RETRIES.fetch_add(1, Ordering::Relaxed);
}
