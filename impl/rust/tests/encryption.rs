//! Encryption at rest (spec/design/encryption.md; bytes in spec/fileformat/encrypted.md). The
//! credential is a host-API argument the SQL corpus cannot reach, so its contract lives here: an
//! encrypted file round-trips every page kind (tree nodes, overflow chains, indexes, the free list),
//! stores no plaintext, refuses a wrong or missing credential, detects tampering and replay as
//! `XX001`, survives compaction, and writes the same bytes for the same history under a seeded key
//! source. The cross-core byte contract is pinned by the `encrypted_*` goldens
//! (tests/fileformat_golden.rs). Mirrors impl/go/encryption_test.go and impl/ts/tests/encryption.test.ts.

use std::path::{Path, PathBuf};

use jed::value::Value;
use jed::{
    Argon2Params, CreateOptions, Credential, Database, Environment, OpenOptions, SessionOptions,
};

const PAGE: u32 = 1024;
const KEY: [u8; 32] = [0x4A; 32];

fn tmp(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(sibling(&path, ".lock"));
    let _ = std::fs::remove_file(sibling(&path, ".jedtmp"));
    path
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn seeded_env() -> Environment {
    Environment::builder()
        .insecure_seeded_key_entropy(0x4A45_4442)
        .build()
}

fn create(path: &Path, credential: Credential) -> Database {
    Database::create(CreateOptions {
        path: Some(path.to_path_buf()),
        page_size: PAGE,
        skip_fsync: true,
        environment: seeded_env(),
        credential: Some(credential),
        ..Default::default()
    })
    .expect("create encrypted db")
}

fn open(path: &Path, credential: Option<Credential>) -> jed::Result<Database> {
    Database::open_with_options(
        path,
        OpenOptions {
            skip_fsync: true,
            credential,
            ..OpenOptions::default()
        },
    )
}

fn exec(db: &Database, sql: &str) {
    db.session(SessionOptions::default())
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {}", e.message));
}

fn try_rows(db: &Database, sql: &str) -> jed::Result<Vec<Vec<Value>>> {
    let mut s = db.session(SessionOptions::default());
    let mut rows = s.query(sql, &[])?;
    let out: Vec<Vec<Value>> = rows.by_ref().collect();
    rows.error()?;
    Ok(out)
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    try_rows(db, sql).unwrap_or_else(|e| panic!("{sql}: {}", e.message))
}

const SUMMARY: &str = "SELECT count(*), sum(id), max(note), sum(length(blob)), count(*) FILTER (WHERE span && i32range(10, 20)) FROM t";

/// Every page kind an encrypted file holds: a multi-level table tree, a secondary and a GiST index,
/// incompressible values long enough to spill into overflow chains, and enough churn to persist a
/// free list.
fn populate(db: &Database) {
    populate_but_last(db);
    exec(db, LAST);
}

/// The last commit of [`populate`]: one row, so most live pages are shared with the generation
/// before it and a tampered one is caught by its pointer, not by the newest commit's manifest.
const LAST: &str = "INSERT INTO t VALUES (5000, 'tail', NULL, NULL)";

/// [`populate`] up to its last commit, whose pages tamper tests may knock out (recovery then falls
/// back to this state).
fn populate_but_last(db: &Database) {
    exec(
        db,
        "CREATE TABLE t (id i64 PRIMARY KEY, note text, blob bytea, span i32range)",
    );
    exec(db, "CREATE INDEX t_note ON t (note)");
    exec(db, "CREATE INDEX t_span ON t USING gist (span)");
    exec(
        db,
        "INSERT INTO t SELECT g, 'secret-' || g, NULL, i32range(g, g + 3) FROM generate_series(1, 400) g",
    );
    exec(
        db,
        "UPDATE t SET blob = decode(repeat(md5(id::text), 40), 'hex') WHERE id % 50 = 0",
    );
    exec(db, "DELETE FROM t WHERE id % 3 = 0");
}

fn summary(db: &Database) -> Vec<Vec<Value>> {
    rows(db, SUMMARY)
}

#[test]
fn round_trips_every_page_kind_and_stores_no_plaintext() {
    let path = tmp("enc_round_trip.jed");
    let db = create(&path, Credential::Key(KEY));
    populate(&db);
    let before = summary(&db);
    assert_eq!(before[0][0], Value::Int(268));
    db.close().unwrap();

    let bytes = std::fs::read(&path).unwrap();
    assert!(
        !bytes.windows(7).any(|w| w == b"secret-"),
        "a row value is stored in plaintext"
    );
    assert!(
        !bytes.windows(5).any(|w| w == b"t_not"),
        "the catalog is stored in plaintext"
    );

    let db = open(&path, Some(Credential::Key(KEY))).unwrap();
    assert_eq!(summary(&db), before);
    exec(&db, "INSERT INTO t VALUES (1000, 'later', NULL, NULL)");
    db.close().unwrap();
    let db = open(&path, Some(Credential::Key(KEY))).unwrap();
    assert_eq!(
        rows(&db, "SELECT note FROM t WHERE id = 1000")[0][0],
        Value::Text("later".into())
    );
}

#[test]
fn credentials_fail_closed() {
    let path = tmp("enc_credentials.jed");
    create(&path, Credential::Key(KEY)).close().unwrap();
    let code = |r: jed::Result<Database>| r.err().expect("open must fail").code().to_string();
    assert_eq!(code(open(&path, Some(Credential::Key([1; 32])))), "28P01");
    assert_eq!(
        code(open(&path, Some(Credential::Passphrase("x".into())))),
        "28P01"
    );
    assert_eq!(code(open(&path, None)), "28P02");

    let plain = tmp("enc_plain.jed");
    Database::create(CreateOptions {
        path: Some(plain.clone()),
        page_size: PAGE,
        skip_fsync: true,
        ..Default::default()
    })
    .unwrap()
    .close()
    .unwrap();
    assert_eq!(code(open(&plain, Some(Credential::Key(KEY)))), "28P03");

    let memory = Database::create(CreateOptions {
        credential: Some(Credential::Key(KEY)),
        ..Default::default()
    });
    assert_eq!(code(memory), "55000");

    let small = Database::create(CreateOptions {
        path: Some(tmp("enc_small.jed")),
        page_size: 512,
        skip_fsync: true,
        credential: Some(Credential::Key(KEY)),
        ..Default::default()
    });
    assert_eq!(code(small), "0A000");
}

#[test]
fn passphrase_unlocks_and_the_key_is_redacted() {
    let path = tmp("enc_passphrase.jed");
    let db = Database::create(CreateOptions {
        path: Some(path.clone()),
        page_size: PAGE,
        skip_fsync: true,
        credential: Some(Credential::Passphrase("correct horse".into())),
        kdf: Argon2Params {
            memory_kib: 64,
            iterations: 1,
            parallelism: 1,
        },
        ..Default::default()
    })
    .unwrap();
    exec(&db, "CREATE TABLE t (id i32 PRIMARY KEY)");
    exec(&db, "INSERT INTO t VALUES (7)");
    db.close().unwrap();
    let db = open(&path, Some(Credential::Passphrase("correct horse".into()))).unwrap();
    assert_eq!(rows(&db, "SELECT id FROM t")[0][0], Value::Int(7));
    db.close().unwrap();
    assert_eq!(
        open(&path, Some(Credential::Passphrase("wrong".into())))
            .err()
            .unwrap()
            .code(),
        "28P01"
    );
    let shown = format!("{:?}", Credential::Passphrase("correct horse".into()));
    assert!(!shown.contains("horse"), "{shown}");
}

#[test]
fn tampering_and_replay_are_detected() {
    let path = tmp("enc_tamper.jed");
    let db = create(&path, Credential::Key(KEY));
    populate_but_last(&db);
    let fallback = summary(&db);
    exec(&db, LAST);
    db.close().unwrap();
    let original = std::fs::read(&path).unwrap();
    let ps = PAGE as usize;

    // Flip one ciphertext byte of body pages in turn: each open or full read fails `XX001`, never
    // touched the page, or — for a page the newest commit wrote, which recovery checks against its
    // manifest — falls back to the previous generation (encrypted.md §4). Never other rows.
    let expected = {
        let db = open(&path, Some(Credential::Key(KEY))).unwrap();
        summary(&db)
    };
    let pages = original.len() / ps;
    for page in (4..pages).step_by(5).chain([pages - 1]) {
        let mut bytes = original.clone();
        bytes[page * ps + 40] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        match open(&path, Some(Credential::Key(KEY))) {
            Err(e) => assert_eq!(e.code(), "XX001", "page {page}"),
            Ok(db) => match try_rows(&db, SUMMARY) {
                Err(e) => assert_eq!(e.code(), "XX001", "page {page}"),
                Ok(got) => assert!(got == expected || got == fallback, "page {page}: {got:?}"),
            },
        }
    }

    // Replay: copy a genuine body page over another index. A per-page AEAD alone would accept it;
    // the pointer binding names the overwritten page's tag, so a read that reaches it fails.
    let mut detected = 0;
    for b in (5..pages).step_by(7) {
        let mut bytes = original.clone();
        let copy = bytes[(b - 1) * ps..b * ps].to_vec();
        bytes[b * ps..(b + 1) * ps].copy_from_slice(&copy);
        std::fs::write(&path, &bytes).unwrap();
        match open(&path, Some(Credential::Key(KEY))).and_then(|db| try_rows(&db, SUMMARY)) {
            Err(e) => {
                assert_eq!(e.code(), "XX001", "page {b}");
                detected += 1;
            }
            Ok(got) => assert!(got == expected || got == fallback, "page {b}: {got:?}"),
        }
    }
    assert!(detected > 0, "no replay reached a live page");
    std::fs::write(&path, &original).unwrap();
}

#[test]
fn compaction_keeps_the_key_and_reseals() {
    let path = tmp("enc_compact.jed");
    let db = create(&path, Credential::Key(KEY));
    populate(&db);
    let before = summary(&db);
    let key_block = std::fs::read(&path).unwrap()[2 * PAGE as usize..4 * PAGE as usize].to_vec();
    db.compact("main").unwrap();
    assert_eq!(summary(&db), before);
    db.close().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes[2 * PAGE as usize..4 * PAGE as usize], key_block[..]);
    let db = open(&path, Some(Credential::Key(KEY))).unwrap();
    assert_eq!(summary(&db), before);
}

#[test]
fn seeded_history_writes_identical_bytes() {
    let image = |name: &str| {
        let path = tmp(name);
        let db = create(&path, Credential::Key(KEY));
        populate(&db);
        db.close().unwrap();
        std::fs::read(&path).unwrap()
    };
    assert_eq!(image("enc_seed_a.jed"), image("enc_seed_b.jed"));
}
