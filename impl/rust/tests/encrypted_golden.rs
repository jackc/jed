//! The encrypted variant's cross-core byte contract (spec/fileformat/encrypted.md; CLAUDE.md §8):
//! each `encrypted_*` golden is the from-scratch image of a logical database under the goldens' host
//! key, with the key material and every page nonce drawn from the insecure seeded stream. This core
//! must (a) WRITE the same logical database — built in an encrypted file, so its trees split at the
//! encrypted capacity — to exactly the golden's bytes, and (b) READ the golden back with the key.
//! The goldens are authored by spec/fileformat/verify.rb. Mirrors impl/go/encrypted_golden_test.go
//! and impl/ts/tests/encrypted_golden.test.ts.

use std::path::{Path, PathBuf};

use jed::value::Value;
use jed::{CreateOptions, Credential, Database, OpenOptions, SessionOptions};

const PAGE: u32 = 1024;
const ENC_KEY: [u8; 32] = [0x4A; 32];
const ENC_SEED: u64 = 0x4A45_4442;

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec/fileformat/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn tmp(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = std::fs::remove_dir_all(PathBuf::from(lock));
    path
}

fn create(path: &Path) -> Database {
    Database::create(CreateOptions {
        path: Some(path.to_path_buf()),
        page_size: PAGE,
        skip_fsync: true,
        credential: Some(Credential::Key(ENC_KEY)),
        ..Default::default()
    })
    .expect("create encrypted db")
}

fn run(db: &Database, sql: &str) {
    db.session(SessionOptions::default())
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message));
}

fn query(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    let mut s = db.session(SessionOptions::default());
    let mut rows = s
        .query(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message));
    let out: Vec<Vec<Value>> = rows.by_ref().collect();
    rows.error().unwrap();
    out
}

const ALPHA64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The format.md `filler64` incompressible filler (xorshift32 seeded `JEDB`).
fn filler(n: usize) -> Vec<u32> {
    let mut x: u32 = 0x4A45_4442;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        })
        .collect()
}

fn filler_text(n: usize) -> String {
    filler(n)
        .into_iter()
        .map(|x| ALPHA64[(x % 64) as usize] as char)
        .collect()
}

fn filler_bytes_hex(n: usize) -> String {
    filler(n)
        .into_iter()
        .map(|x| format!("{:02x}", x % 256))
        .collect()
}

/// Build `name`'s logical database with `sql`, compare its sealed image with the golden, and read
/// the golden back, checking `probe` against what the built database returns.
fn check(name: &str, sql: &[String], probe: &str) {
    let path = tmp(&format!("golden_{name}"));
    let db = create(&path);
    for stmt in sql {
        run(&db, stmt);
    }
    let want = query(&db, probe);
    let image = db
        .encrypted_image_for_testing(ENC_SEED, &Credential::Key(ENC_KEY))
        .unwrap();
    db.close().unwrap();
    let golden = fixture(name);
    assert!(
        image == golden,
        "{name}: the sealed image differs from the golden"
    );

    let copy = tmp(&format!("read_{name}"));
    std::fs::write(&copy, &golden).unwrap();
    let db = Database::open_with_options(
        &copy,
        OpenOptions {
            credential: Some(Credential::Key(ENC_KEY)),
            ..OpenOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("open {name}: {}", e.message));
    assert_eq!(query(&db, probe), want, "{name}: rows read back");
}

#[test]
fn encrypted_empty_db() {
    check("encrypted_empty_db.jed", &[], "SELECT 1");
}

#[test]
fn encrypted_pk_table() {
    let mut sql = vec!["CREATE TABLE t (id i32 PRIMARY KEY, v i16)".to_string()];
    for i in 1..=300 {
        let v = if i == 3 {
            "NULL".to_string()
        } else {
            (i * 10).to_string()
        };
        sql.push(format!("INSERT INTO t VALUES ({i}, {v})"));
    }
    check(
        "encrypted_pk_table.jed",
        &sql,
        "SELECT count(*), sum(id), sum(v), count(v) FROM t",
    );
}

#[test]
fn encrypted_overflow_table() {
    let sql = vec![
        "CREATE TABLE t (id i32 PRIMARY KEY, body text, blob bytea)".to_string(),
        format!(
            "INSERT INTO t VALUES (1, '{}', '\\x{}')",
            filler_text(2500),
            filler_bytes_hex(1200)
        ),
        "INSERT INTO t VALUES (2, 'small', '\\xcafe')".to_string(),
        "INSERT INTO t VALUES (3, NULL, NULL)".to_string(),
    ];
    check(
        "encrypted_overflow_table.jed",
        &sql,
        "SELECT id, md5(body), md5(blob) FROM t ORDER BY id",
    );
}

#[test]
fn encrypted_index_table() {
    let mut sql = vec![
        "CREATE TABLE t (a i32 PRIMARY KEY, v i64, r i32range)".to_string(),
        "CREATE INDEX t_r ON t USING gist (r)".to_string(),
        "CREATE INDEX t_v ON t (v)".to_string(),
    ];
    for a in 1..=150 {
        sql.push(format!(
            "INSERT INTO t VALUES ({a}, {}, i32range({a}, {}))",
            a * 3,
            a + 5
        ));
    }
    check(
        "encrypted_index_table.jed",
        &sql,
        "SELECT count(*) FILTER (WHERE r && i32range(40, 44)), count(*) FILTER (WHERE v = 300), sum(v) FROM t",
    );
}

#[test]
fn encrypted_catalog() {
    let sql: Vec<String> = (0..30)
        .map(|i| format!("CREATE TABLE t{i:02} (id i32 PRIMARY KEY, a text, b i64)"))
        .collect();
    check("encrypted_catalog.jed", &sql, "SELECT count(*) FROM t29");
}
