//! Test-only line actor for the shared real-process corpus (concurrency-testing.md §10).

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use jed::{
    AttachSource, CreateOptions, Database, EngineError, Locking, OpenOptions, Session,
    SessionOptions, Value,
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
    let timeout = args.next().and_then(|s| s.parse().ok()).unwrap_or(5000);
    let mut database = if action == "create" {
        Database::create(CreateOptions {
            path: Some(path),
            locking: Locking::Shared,
            file_lock_timeout_ms: timeout,
            ..CreateOptions::default()
        })?
    } else {
        Database::open_with_options(
            path,
            OpenOptions {
                locking: Locking::Shared,
                file_lock_timeout_ms: timeout,
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
            "TXID" => Ok(database.txid().to_string()),
            "PAGE_COUNT" => Ok(database.page_count().to_string()),
            "CLOSE" => {
                drop(reader.take());
                drop(writer.take());
                database.close()?;
                reply_ok("");
                return Ok(());
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
