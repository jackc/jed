//! Test-only line actor for the shared real-process corpus (concurrency-testing.md §10).

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use jed::{
    AttachSource, CreateOptions, Database, EngineError, Environment, Locking, OpenOptions,
    Ownership, Session, SessionOptions, Value,
};

fn main() {
    if let Err(error) = run() {
        reply_error(&error);
        std::process::exit(1);
    }
}

fn run() -> Result<(), EngineError> {
    let mut args = std::env::args().skip(1);
    let action = args.next().expect("usage: process_actor create|open PATH");
    let path = PathBuf::from(args.next().expect("usage: process_actor create|open PATH"));
    if action == "owner-idle" {
        // The corpus-through-a-client driver (spec/pageservice/README.md §7): open and close
        // databases as their page-service owner on command. PATH is the repository root.
        return owner_idle(&path);
    }
    // Positional arguments after the path: the open timeout, then the page-service owner's
    // idle-in-transaction timeout (spec/design/page-service.md §11.1). key=value arguments set the
    // handoff options (spec/pageservice/README.md §6.5): ownership=prefer|yield|never, yield_grace,
    // shutdown_grace, idle_session (milliseconds), max_connections.
    let mut positional = Vec::new();
    let mut options = std::collections::HashMap::new();
    for arg in args {
        match arg.split_once('=') {
            Some((key, value)) => {
                options.insert(key.to_string(), value.to_string());
            }
            None => positional.push(arg),
        }
    }
    let timeout = positional
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5000);
    let idle = positional
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(jed::DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT_MS);
    let millis = |key: &str| options.get(key).map(|v| v.parse().unwrap_or(0));
    let ownership = match options.get("ownership").map(String::as_str) {
        Some("yield") => Ownership::Yield,
        Some("never") => Ownership::Never,
        _ => Ownership::Prefer,
    };
    let max_connections = options
        .get("max_connections")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // create/open use the default locking (the page service); open-exclusive is a non-serving owner.
    let locking = match action.as_str() {
        "open-exclusive" => Locking::Exclusive,
        _ => Locking::Auto,
    };
    let mut database = if action == "create" {
        Database::create(CreateOptions {
            path: Some(path),
            locking,
            file_lock_timeout_ms: timeout,
            idle_in_transaction_timeout_ms: idle,
            idle_session_timeout_ms: millis("idle_session"),
            yield_grace_ms: millis("yield_grace"),
            shutdown_grace_ms: millis("shutdown_grace"),
            max_connections,
            ownership,
            ..CreateOptions::default()
        })?
    } else {
        Database::open_with_options(
            path,
            OpenOptions {
                locking,
                file_lock_timeout_ms: timeout,
                idle_in_transaction_timeout_ms: idle,
                read_only: action == "open-read-only",
                idle_session_timeout_ms: millis("idle_session"),
                yield_grace_ms: millis("yield_grace"),
                shutdown_grace_ms: millis("shutdown_grace"),
                max_connections,
                ownership,
                ..OpenOptions::default()
            },
        )?
    };
    println!("READY");
    io::stdout().flush().expect("flush READY");

    let mut reader: Option<Session> = None;
    let mut writer: Option<Session> = None;
    for line in io::stdin().lock().lines() {
        let line = line.expect("read actor command");
        let (command, argument) = line.split_once('\t').unwrap_or((&line, ""));
        let result = match command {
            "EXEC" => {
                decode_sql(argument).and_then(|sql| database.execute(&sql, &[]).map(|_| "".into()))
            }
            "ATTACH" => {
                let mut parts = argument.splitn(3, '\t');
                let name = parts.next().expect("ATTACH name");
                let read_only = parts.next().expect("ATTACH read-only") == "1";
                let path = parts.next().expect("ATTACH path");
                database
                    .attach(name, AttachSource::file(path), read_only)
                    .map(|_| String::new())
            }
            "DETACH" => database.detach(argument).map(|_| String::new()),
            "QUERY_I64" => decode_sql(argument).and_then(|sql| query_i64(&mut database, &sql)),
            "READ_OPEN" => {
                reader = Some(database.read_session());
                Ok(String::new())
            }
            "READ_QUERY_I64" => decode_sql(argument).and_then(|sql| {
                query_i64(
                    reader.as_mut().expect("READ_OPEN precedes READ_QUERY_I64"),
                    &sql,
                )
            }),
            "READ_CLOSE" => {
                if let Some(mut session) = reader.take() {
                    session.close();
                }
                Ok(String::new())
            }
            "WRITE_OPEN" => {
                let mut session = database.session(SessionOptions::default());
                session.set_lock_timeout_ms(argument.parse().unwrap_or(0));
                match session.begin(true) {
                    Ok(()) => {
                        writer = Some(session);
                        Ok(String::new())
                    }
                    Err(error) => Err(error),
                }
            }
            "WRITE_EXEC" => decode_sql(argument).and_then(|sql| {
                writer
                    .as_mut()
                    .expect("WRITE_OPEN precedes WRITE_EXEC")
                    .execute(&sql, &[])
                    .map(|_| String::new())
            }),
            "WRITE_COMMIT" => writer
                .as_mut()
                .expect("WRITE_OPEN precedes WRITE_COMMIT")
                .commit()
                .map(|_| String::new()),
            "WRITE_ROLLBACK" => writer
                .as_mut()
                .expect("WRITE_OPEN precedes WRITE_ROLLBACK")
                .rollback()
                .map(|_| String::new()),
            "COMPACT" => database
                .compact(if argument.is_empty() {
                    "main"
                } else {
                    argument
                })
                .map(|_| String::new()),
            // The backup test entry point (spec/design/page-service.md §9): BACKUP <form>\t<dest>.
            "BACKUP" => {
                let (form, dest) = argument.split_once('\t').unwrap_or((argument, ""));
                database
                    .backup_for_testing(dest, form == "page_identical")
                    .map(|_| String::new())
            }
            "MAX_STORAGE_BYTES" => database
                .set_max_storage_bytes("main", argument.parse().expect("MAX_STORAGE_BYTES bytes"))
                .map(|_| String::new()),
            "STORAGE_BYTES" => database
                .storage_bytes("main")
                .map(|bytes| bytes.to_string()),
            // The page-service benchmark's timed loops (spec/design/benchmarks.md §12).
            "TIME_LOOKUPS" | "TIME_SCAN" | "TIME_COMMITS" => {
                timed(&mut database, command, argument)
            }
            "HANGUP_NEXT_ACK" => {
                // The commit-resolution test hook (spec/pageservice/README.md §6.4).
                database.hang_up_next_page_service_ack_for_testing();
                Ok(String::new())
            }
            "CRASH_NEXT_ACK" => {
                // Exit instead of acknowledging the next client commit (spec/pageservice/README.md
                // §6.5).
                database.crash_before_next_page_service_ack_for_testing();
                Ok(String::new())
            }
            // The page-service role test hook (spec/pageservice/README.md §6.5): main's, or an
            // attachment's when the step names one.
            "ROLE" if argument.is_empty() => {
                Ok(database.page_service_role_for_testing().to_string())
            }
            "ROLE" => Ok(database
                .page_service_attachment_role_for_testing(argument)
                .to_string()),
            "TXID" => Ok(database.txid().to_string()),
            "PAGE_COUNT" => Ok(database.page_count().to_string()),
            "CLOSE" => {
                // Acknowledged at once: an owner then drains its clients before it exits
                // (spec/pageservice/README.md §6.5), and the harness waits for the exit status.
                drop(reader.take());
                drop(writer.take());
                reply_ok("");
                return database.close();
            }
            other => panic!("unknown actor command {other}"),
        };
        match result {
            Ok(value) => reply_ok(&value),
            Err(error) => reply_error(&error),
        }
    }
    Ok(())
}

/// Serve `OWNER_OPEN <hex path>` / `OWNER_CLOSE` / `CLOSE`: own one database at a time under the
/// page service, so another core's conformance harness can run queries as its client.
fn owner_idle(root: &std::path::Path) -> Result<(), EngineError> {
    let environment = owner_environment(root)?;
    println!("READY");
    io::stdout().flush().expect("flush READY");
    let mut database: Option<Database> = None;
    for line in io::stdin().lock().lines() {
        let line = line.expect("read actor command");
        let (command, argument) = line.split_once('\t').unwrap_or((&line, ""));
        let result = match command {
            "OWNER_OPEN" => decode_sql(argument).and_then(|path| {
                Database::open_with_options(
                    path,
                    OpenOptions {
                        locking: Locking::Auto,
                        skip_fsync: true,
                        file_lock_timeout_ms: 5000,
                        environment: environment.clone(),
                        ..OpenOptions::default()
                    },
                )
                .map(|db| database = Some(db))
            }),
            "OWNER_CLOSE" => match database.take() {
                Some(db) => db.close(),
                None => Ok(()),
            },
            "CLOSE" => {
                if let Some(db) = database.take() {
                    let _ = db.close();
                }
                reply_ok("");
                return Ok(());
            }
            other => panic!("unknown owner-idle command {other}"),
        };
        match result {
            Ok(()) => reply_ok(""),
            Err(error) => reply_error(&error),
        }
    }
    Ok(())
}

/// The spec's Unicode and time-zone bundles: like any opener, an owner must resolve the collations
/// a file's catalog references (`XX002` otherwise), and the corpus files use these.
fn owner_environment(root: &std::path::Path) -> Result<Environment, EngineError> {
    let read =
        |rel: &str| std::fs::read(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"));
    Ok(Environment::builder()
        .load_unicode_data(&read("spec/collation/fixtures/unicode.jucd"))?
        .load_time_zone_data(&read("spec/tz/fixtures/tzdata.jtz"))?
        .build())
}

/// Runs one of the page-service benchmark's loops (spec/design/benchmarks.md §12) and returns
/// "<elapsed ns>:<checksum>". `argument` is "<iterations>[\t<rows>]". Probe k reads id
/// (k * 7919) mod rows + 1, so every core runs the same workload; the checksum is the sum of the
/// values read (TIME_LOOKUPS), the last scan's sum (TIME_SCAN), or 0 (TIME_COMMITS).
fn timed(database: &mut Database, command: &str, argument: &str) -> Result<String, EngineError> {
    let mut fields = argument.split('\t');
    let n: i64 = fields
        .next()
        .and_then(|s| s.parse().ok())
        .expect("iterations");
    let rows: i64 = fields.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let mut checksum = 0i64;
    let start = std::time::Instant::now();
    let int = |value: &Value| match value {
        Value::Int(v) => *v,
        other => panic!("expected an integer, got {other:?}"),
    };
    match command {
        "TIME_LOOKUPS" => {
            let stmt = database.prepare("SELECT v FROM bench_ps WHERE id = $1")?;
            for k in 0..n {
                let mut rows_out =
                    database.query_prepared(&stmt, &[Value::Int(k * 7919 % rows + 1)])?;
                for row in rows_out.by_ref() {
                    checksum += int(&row[0]);
                }
                rows_out.error()?;
            }
        }
        "TIME_SCAN" => {
            for _ in 0..n {
                let mut rows_out = database.query("SELECT sum(v)::i64 FROM bench_ps", &[])?;
                for row in rows_out.by_ref() {
                    checksum = int(&row[0]);
                }
                rows_out.error()?;
            }
        }
        _ => {
            let stmt = database.prepare("UPDATE bench_ps SET v = v + 1 WHERE id = $1")?;
            for k in 0..n {
                database.execute_prepared(&stmt, &[Value::Int(k * 7919 % rows + 1)])?;
            }
        }
    }
    Ok(format!("{}:{checksum}", start.elapsed().as_nanos()))
}

trait QueryHandle {
    fn query_actor(&mut self, sql: &str) -> Result<jed::Rows, EngineError>;
}

impl QueryHandle for Database {
    fn query_actor(&mut self, sql: &str) -> Result<jed::Rows, EngineError> {
        self.query(sql, &[])
    }
}

impl QueryHandle for Session {
    fn query_actor(&mut self, sql: &str) -> Result<jed::Rows, EngineError> {
        self.query(sql, &[])
    }
}

fn query_i64(handle: &mut impl QueryHandle, sql: &str) -> Result<String, EngineError> {
    let mut rows = handle.query_actor(sql)?;
    let mut rendered = Vec::new();
    for row in rows.by_ref() {
        let fields = row
            .into_iter()
            .map(|value| match value {
                Value::Int(value) => value.to_string(),
                Value::Null => "NULL".to_string(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join(":");
        rendered.push(fields);
    }
    rows.error()?;
    Ok(rendered.join(","))
}

fn decode_sql(value: &str) -> Result<String, EngineError> {
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let text = std::str::from_utf8(pair).expect("hex is ASCII");
        bytes.push(u8::from_str_radix(text, 16).expect("command SQL is hex"));
    }
    Ok(String::from_utf8(bytes).expect("command SQL is UTF-8"))
}

fn reply_ok(value: &str) {
    println!("OK\t{value}");
    io::stdout().flush().expect("flush actor response");
}

fn reply_error(error: &EngineError) {
    println!("ERR\t{}\t{}", error.code(), hex(error.message.as_bytes()));
    io::stdout().flush().expect("flush actor error");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
