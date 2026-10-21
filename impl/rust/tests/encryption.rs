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

fn key_pages(path: &Path) -> (Vec<u8>, Vec<u8>) {
    let bytes = std::fs::read(path).unwrap();
    let ps = PAGE as usize;
    (
        bytes[2 * ps..3 * ps].to_vec(),
        bytes[3 * ps..4 * ps].to_vec(),
    )
}

fn set_key_pages(path: &Path, page2: &[u8], page3: &[u8]) {
    let mut bytes = std::fs::read(path).unwrap();
    let ps = PAGE as usize;
    bytes[2 * ps..3 * ps].copy_from_slice(page2);
    bytes[3 * ps..4 * ps].copy_from_slice(page3);
    std::fs::write(path, bytes).unwrap();
}

fn small_kdf() -> Argon2Params {
    Argon2Params {
        memory_kib: 64,
        iterations: 1,
        parallelism: 1,
    }
}

#[test]
fn changing_the_credential_rewraps_the_key_block_only() {
    let path = tmp("enc_change.jed");
    let db = create(&path, Credential::Key(KEY));
    populate(&db);
    let before = summary(&db);
    let body = std::fs::read(&path).unwrap()[4 * PAGE as usize..].to_vec();
    let pass = Credential::Passphrase("new phrase".into());
    db.change_credential("main", pass.clone(), small_kdf())
        .unwrap();
    // The handle keeps working, and no body page was rewritten.
    assert_eq!(summary(&db), before);
    db.close().unwrap();
    assert_eq!(std::fs::read(&path).unwrap()[4 * PAGE as usize..], body[..]);
    let (page2, page3) = key_pages(&path);
    assert_eq!(page2, page3);
    assert_eq!(page2[8..16], 2u64.to_be_bytes());

    assert_eq!(
        open(&path, Some(Credential::Key(KEY)))
            .err()
            .unwrap()
            .code(),
        "28P01"
    );
    let db = open(&path, Some(pass)).unwrap();
    assert_eq!(summary(&db), before);
    // A second change, back to a host key, and a write after it.
    db.change_credential("main", Credential::Key([7; 32]), Argon2Params::default())
        .unwrap();
    exec(&db, "INSERT INTO t VALUES (6000, 'after', NULL, NULL)");
    db.close().unwrap();
    let db = open(&path, Some(Credential::Key([7; 32]))).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0][0], Value::Int(269));
    assert_eq!(key_pages(&path).0[8..16], 3u64.to_be_bytes());
}

#[test]
fn credential_change_errors() {
    let code = |r: jed::Result<()>| r.err().expect("must fail").code().to_string();
    let path = tmp("enc_change_errors.jed");
    let db = create(&path, Credential::Key(KEY));
    let new = || Credential::Key([2; 32]);
    assert_eq!(
        code(db.change_credential("nope", new(), Argon2Params::default())),
        "42704"
    );
    assert_eq!(
        code(db.change_credential("temp", new(), Argon2Params::default())),
        "42704"
    );
    let bad = Argon2Params {
        memory_kib: 1,
        iterations: 1,
        parallelism: 1,
    };
    assert_eq!(
        code(db.change_credential("main", Credential::Passphrase("p".into()), bad)),
        "22023"
    );
    exec(&db, "CREATE TABLE t (id i32 PRIMARY KEY)");
    let mut writer = db.session(SessionOptions::default());
    writer.execute("BEGIN", &[]).unwrap();
    writer.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
    assert_eq!(
        code(db.change_credential("main", new(), Argon2Params::default())),
        "55006"
    );
    writer.execute("ROLLBACK", &[]).unwrap();
    drop(writer);
    db.close().unwrap();

    let ro = Database::open_with_options(
        &path,
        OpenOptions {
            skip_fsync: true,
            read_only: true,
            credential: Some(Credential::Key(KEY)),
            ..OpenOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        code(ro.change_credential("main", new(), Argon2Params::default())),
        "25006"
    );
    ro.close().unwrap();

    let memory = Database::create(CreateOptions::default()).unwrap();
    assert_eq!(
        code(memory.change_credential("main", new(), Argon2Params::default())),
        "55000"
    );
    let plain = tmp("enc_change_plain.jed");
    let plain = Database::create(CreateOptions {
        path: Some(plain),
        page_size: PAGE,
        skip_fsync: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        code(plain.change_credential("main", new(), Argon2Params::default())),
        "28P03"
    );
}

#[test]
fn an_attachment_changes_its_own_credential() {
    let path = tmp("enc_change_attach.jed");
    let db = create(&path, Credential::Key(KEY));
    exec(&db, "CREATE TABLE t (id i32 PRIMARY KEY)");
    exec(&db, "INSERT INTO t VALUES (1)");
    db.close().unwrap();
    // An encrypted attachment joins a handle that cannot spill: an encrypted main.
    let main = create(&tmp("enc_change_main.jed"), Credential::Key([3; 32]));
    main.attach(
        "other",
        jed::AttachSource::file(&path).credential(Credential::Key(KEY)),
        false,
    )
    .unwrap();
    main.change_credential("other", Credential::Key([4; 32]), Argon2Params::default())
        .unwrap();
    main.close().unwrap();
    assert_eq!(
        open(&path, Some(Credential::Key(KEY)))
            .err()
            .unwrap()
            .code(),
        "28P01"
    );
    let db = open(&path, Some(Credential::Key([4; 32]))).unwrap();
    assert_eq!(rows(&db, "SELECT id FROM t")[0][0], Value::Int(1));
}

/// A crash during a credential change (encryption.md §5): the copy that is not selected is written
/// first, so a crash before the first barrier leaves the old credential working, and one after it
/// the new — never neither. A writable open repairs a differing pair; a read-only one leaves it.
#[test]
fn an_interrupted_credential_change_leaves_exactly_one_credential() {
    let path = tmp("enc_change_crash.jed");
    let db = create(&path, Credential::Key(KEY));
    exec(&db, "CREATE TABLE t (id i32 PRIMARY KEY)");
    exec(&db, "INSERT INTO t VALUES (1)");
    db.close().unwrap();
    let (old, _) = key_pages(&path);
    let db = open(&path, Some(Credential::Key(KEY))).unwrap();
    db.change_credential("main", Credential::Key([8; 32]), Argon2Params::default())
        .unwrap();
    db.close().unwrap();
    let (new, _) = key_pages(&path);

    // Torn first write (page 3, the unselected copy of an equal pair): the old credential.
    let mut torn = new.clone();
    torn[100..].fill(0xEE);
    set_key_pages(&path, &old, &torn);
    assert_eq!(
        open(&path, Some(Credential::Key([8; 32])))
            .err()
            .unwrap()
            .code(),
        "28P01"
    );
    open(&path, Some(Credential::Key(KEY)))
        .unwrap()
        .close()
        .unwrap();
    assert_eq!(
        key_pages(&path),
        (old.clone(), old.clone()),
        "the open repairs the torn copy"
    );

    // The first write landed, the second did not: the new credential, and a read-only open
    // leaves the pair as it is.
    set_key_pages(&path, &old, &new);
    assert_eq!(
        open(&path, Some(Credential::Key(KEY)))
            .err()
            .unwrap()
            .code(),
        "28P01"
    );
    let ro = Database::open_with_options(
        &path,
        OpenOptions {
            skip_fsync: true,
            read_only: true,
            credential: Some(Credential::Key([8; 32])),
            ..OpenOptions::default()
        },
    )
    .unwrap();
    assert_eq!(rows(&ro, "SELECT id FROM t")[0][0], Value::Int(1));
    ro.close().unwrap();
    assert_eq!(key_pages(&path), (old.clone(), new.clone()));
    open(&path, Some(Credential::Key([8; 32])))
        .unwrap()
        .close()
        .unwrap();
    assert_eq!(key_pages(&path), (new.clone(), new.clone()));

    // Two valid copies of one generation that differ are tampering.
    let mut forged = new.clone();
    forged[40] ^= 1;
    let crc = crc32(&forged[..124]);
    forged[124..128].copy_from_slice(&crc.to_be_bytes());
    set_key_pages(&path, &new, &forged);
    assert_eq!(
        open(&path, Some(Credential::Key([8; 32])))
            .err()
            .unwrap()
            .code(),
        "XX001"
    );
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn scratch_files(dir: &Path) -> Vec<Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| std::fs::read(e.unwrap().path()).unwrap_or_default())
        .collect()
}

/// Scratch encryption (encryption.md §6): an encrypted database spills its sorts, hash operators
/// and temp tables to sealed scratch files, with the same rows and cost as running in memory and no
/// plaintext in the scratch directory.
#[test]
fn an_encrypted_database_spills_to_sealed_scratch() {
    let dir = tmp("enc_scratch_dir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = tmp("enc_scratch.jed");
    let db = create(&path, Credential::Key(KEY));
    db.set_spill_dir_for_test(dir.clone());
    exec(&db, "CREATE TABLE t (id i64 PRIMARY KEY, k i32, s text)");
    exec(
        &db,
        "INSERT INTO t SELECT g, g % 37, 'secret-' || (g * 7919 % 1000) FROM generate_series(1, 3000) g",
    );
    let mut session = db.session(SessionOptions::default());
    let queries = [
        "SELECT s, id FROM t ORDER BY s DESC, id",
        "SELECT k, count(DISTINCT s), max(s) FROM t GROUP BY k ORDER BY k",
        "SELECT a.id, b.id FROM t a JOIN t b ON a.s = b.s ORDER BY a.id, b.id",
    ];
    for sql in queries {
        session.set_work_mem(0);
        let resident = session.query_outcome(sql, &[]).unwrap();
        session.set_work_mem(256);
        let mut cursor = session.query(sql, &[]).unwrap();
        assert!(cursor.next().is_some());
        let files = scratch_files(&dir);
        assert!(!files.is_empty(), "{sql}: nothing spilled");
        for bytes in &files {
            assert!(
                !bytes.windows(7).any(|w| w == b"secret-"),
                "{sql}: plaintext in scratch"
            );
        }
        cursor.close();
        assert_eq!(session.query_outcome(sql, &[]).unwrap(), resident, "{sql}");
    }
    // A temp table past temp_buffers spills its pages to a sealed scratch store.
    let spills = jed::tooling::TEMP_SPILLS.load(std::sync::atomic::Ordering::Relaxed);
    session.set_temp_buffers(8192);
    session
        .execute("CREATE TEMP TABLE tt AS SELECT id, s FROM t", &[])
        .unwrap();
    assert!(jed::tooling::TEMP_SPILLS.load(std::sync::atomic::Ordering::Relaxed) > spills);
    for bytes in scratch_files(&dir) {
        assert!(
            !bytes.windows(7).any(|w| w == b"secret-"),
            "plaintext temp page"
        );
    }
    let mut q = session
        .query("SELECT count(*), max(s) FROM tt", &[])
        .unwrap();
    let row = q.next().unwrap();
    assert_eq!(row[0], Value::Int(3000));
    drop(q);
    drop(session);
    db.close().unwrap();
}

/// An encrypted attachment may not join a handle whose scratch files are plaintext; a handle opened
/// with `scratch_encryption = On` takes it (encryption.md §6).
#[test]
fn scratch_encryption_gates_encrypted_attachments() {
    let enc = tmp("enc_scratch_att.jed");
    create(&enc, Credential::Key(KEY)).close().unwrap();
    let plain_options = |name: &str, scratch| CreateOptions {
        path: Some(tmp(name)),
        page_size: PAGE,
        skip_fsync: true,
        scratch_encryption: scratch,
        ..Default::default()
    };
    let attach = |db: &Database| {
        db.attach(
            "e",
            jed::AttachSource::file(&enc).credential(Credential::Key(KEY)),
            false,
        )
    };
    let plain = Database::create(plain_options(
        "enc_scratch_p1.jed",
        jed::ScratchEncryption::Auto,
    ))
    .unwrap();
    assert_eq!(attach(&plain).err().unwrap().code(), "55000");
    plain.close().unwrap();
    let sealed = Database::create(plain_options(
        "enc_scratch_p2.jed",
        jed::ScratchEncryption::On,
    ))
    .unwrap();
    attach(&sealed).unwrap();
    sealed.close().unwrap();
}

fn sealed_handle(path: &Path, credential: Option<Credential>) -> Database {
    Database::create(CreateOptions {
        path: Some(path.to_path_buf()),
        page_size: PAGE,
        skip_fsync: true,
        environment: seeded_env(),
        credential,
        scratch_encryption: jed::ScratchEncryption::On,
        locking: jed::Locking::Exclusive,
        ..Default::default()
    })
    .unwrap()
}

/// Rotating the data key, encrypting a plaintext file and decrypting an encrypted one
/// (encryption.md §5): every tree rebuilt under the target layout, the rows unchanged.
#[test]
fn rekey_rotates_encrypts_and_decrypts() {
    let path = tmp("enc_rekey.jed");
    let db = sealed_handle(&path, None);
    populate(&db);
    let before = summary(&db);

    // Encrypt the plaintext file in place.
    db.rekey("main", Some(Credential::Key(KEY)), Argon2Params::default())
        .unwrap();
    assert_eq!(summary(&db), before);
    exec(
        &db,
        "INSERT INTO t VALUES (7000, 'after-encrypt', NULL, NULL)",
    );
    db.close().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(!bytes.windows(7).any(|w| w == b"secret-"));
    assert_eq!(open(&path, None).err().unwrap().code(), "28P02");
    let db = open(&path, Some(Credential::Key(KEY))).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0][0], Value::Int(269));
    let key_block = std::fs::read(&path).unwrap()[2 * PAGE as usize..3 * PAGE as usize].to_vec();

    // Rotate: a new data key under a passphrase; the old key no longer opens the file.
    let pass = Credential::Passphrase("rotated".into());
    db.rekey("main", Some(pass.clone()), small_kdf()).unwrap();
    db.close().unwrap();
    let rotated = std::fs::read(&path).unwrap();
    assert_ne!(rotated[2 * PAGE as usize..3 * PAGE as usize], key_block[..]);
    assert_eq!(
        rotated[2 * PAGE as usize + 8..2 * PAGE as usize + 16],
        1u64.to_be_bytes()
    );
    assert_eq!(
        open(&path, Some(Credential::Key(KEY)))
            .err()
            .unwrap()
            .code(),
        "28P01"
    );
    let db = open(&path, Some(pass)).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0][0], Value::Int(269));

    // Decrypt.
    db.rekey("main", None, Argon2Params::default()).unwrap();
    assert_eq!(
        rows(&db, "SELECT note FROM t WHERE id = 7000")[0][0],
        Value::Text("after-encrypt".into())
    );
    db.close().unwrap();
    assert_eq!(
        open(&path, Some(Credential::Key(KEY)))
            .err()
            .unwrap()
            .code(),
        "28P03"
    );
    let db = open(&path, None).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0][0], Value::Int(269));
    // Only the summary's table survives every rebuild with its indexes usable.
    assert_eq!(
        rows(&db, "SELECT count(*) FROM t WHERE note = 'secret-10'")[0][0],
        Value::Int(1)
    );
}

#[test]
fn rekey_errors() {
    let code = |r: jed::Result<()>| r.err().expect("must fail").code().to_string();
    let key = || Some(Credential::Key(KEY));
    // A handle whose scratch files are plaintext cannot encrypt.
    let plain = Database::create(CreateOptions {
        path: Some(tmp("enc_rekey_plain.jed")),
        page_size: PAGE,
        skip_fsync: true,
        locking: jed::Locking::Exclusive,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        code(plain.rekey("main", key(), Argon2Params::default())),
        "55000"
    );
    plain.close().unwrap();
    // The owner of a database the page service shares rekeys it (here it encrypts it).
    let served = Database::create(CreateOptions {
        path: Some(tmp("enc_rekey_served.jed")),
        page_size: PAGE,
        skip_fsync: true,
        scratch_encryption: jed::ScratchEncryption::On,
        ..Default::default()
    })
    .unwrap();
    served
        .rekey("main", key(), Argon2Params::default())
        .unwrap();
    served.close().unwrap();
    let small = sealed_handle_page(&tmp("enc_rekey_small.jed"), 512);
    assert_eq!(
        code(small.rekey("main", key(), Argon2Params::default())),
        "0A000"
    );
    let memory = Database::create(CreateOptions::default()).unwrap();
    assert_eq!(
        code(memory.rekey("main", key(), Argon2Params::default())),
        "55000"
    );
    let db = sealed_handle(&tmp("enc_rekey_err.jed"), key());
    assert_eq!(
        code(db.rekey("nope", key(), Argon2Params::default())),
        "42704"
    );
    exec(&db, "CREATE TABLE t (id i32 PRIMARY KEY)");
    exec(&db, "INSERT INTO t VALUES (1), (2)");
    let mut reader = db.session(SessionOptions::default());
    let mut cursor = reader.query("SELECT id FROM t", &[]).unwrap();
    assert!(cursor.next().is_some());
    assert_eq!(
        code(db.rekey("main", None, Argon2Params::default())),
        "55006"
    );
    drop(cursor);
}

fn sealed_handle_page(path: &Path, page_size: u32) -> Database {
    Database::create(CreateOptions {
        path: Some(path.to_path_buf()),
        page_size,
        skip_fsync: true,
        scratch_encryption: jed::ScratchEncryption::On,
        locking: jed::Locking::Exclusive,
        ..Default::default()
    })
    .unwrap()
}

/// A row that fits a plaintext page but not an encrypted one (40 bytes less, wider pointers): the
/// encryption fails before the file is replaced, which still holds every row (encryption.md §5).
#[test]
fn a_rekey_the_target_cannot_store_leaves_the_file_untouched() {
    let path = tmp("enc_rekey_toolong.jed");
    let db = sealed_handle(&path, None);
    exec(&db, "CREATE TABLE t (k text PRIMARY KEY)");
    // The longest key the plaintext layout stores.
    let mut len = 1000;
    while try_rows(
        &db,
        &format!("INSERT INTO t VALUES (repeat('k', {len})) RETURNING 1"),
    )
    .is_err()
    {
        len -= 1;
    }
    let before = std::fs::read(&path).unwrap();
    let err = db
        .rekey("main", Some(Credential::Key(KEY)), Argon2Params::default())
        .err()
        .expect("the key does not fit the encrypted layout");
    assert_ne!(err.code(), "XX001", "{}", err.message);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(rows(&db, "SELECT length(k) FROM t")[0][0], Value::Int(len));
    db.close().unwrap();
    assert!(!sibling(&path, ".jedtmp").exists());
}

/// The same history and rekey write the same bytes under the seeded key source.
#[test]
fn a_seeded_rekey_writes_identical_bytes() {
    let image = |name: &str| {
        let path = tmp(name);
        let db = sealed_handle(&path, Some(Credential::Key(KEY)));
        populate(&db);
        db.rekey(
            "main",
            Some(Credential::Key([6; 32])),
            Argon2Params::default(),
        )
        .unwrap();
        db.close().unwrap();
        std::fs::read(&path).unwrap()
    };
    assert_eq!(image("enc_rekey_seed_a.jed"), image("enc_rekey_seed_b.jed"));
}

/// A rebuild larger than the page-cache budget moves its scratch domain to a sealed scratch file
/// instead of holding every rebuilt page in memory (encryption.md §5, §6).
#[test]
fn a_large_rekey_spills_its_scratch_domain() {
    let path = tmp("enc_rekey_large.jed");
    let dir = tmp("enc_rekey_large_scratch");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = sealed_handle(&path, Some(Credential::Key(KEY)));
    db.close().unwrap();
    let db = Database::open_with_options(
        &path,
        OpenOptions {
            skip_fsync: true,
            cache_bytes: 64 * 1024,
            credential: Some(Credential::Key(KEY)),
            locking: jed::Locking::Exclusive,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    db.set_spill_dir_for_test(dir.clone());
    exec(&db, "CREATE TABLE big (id i64 PRIMARY KEY, s text)");
    exec(
        &db,
        "INSERT INTO big SELECT g, 'secret-' || md5(g::text) || md5((g * 3)::text) FROM generate_series(1, 30000) g",
    );
    let want = rows(&db, "SELECT count(*), max(s), sum(length(s)) FROM big");
    let spills = jed::tooling::TEMP_SPILLS.load(std::sync::atomic::Ordering::Relaxed);
    db.rekey(
        "main",
        Some(Credential::Key([2; 32])),
        Argon2Params::default(),
    )
    .unwrap();
    assert!(jed::tooling::TEMP_SPILLS.load(std::sync::atomic::Ordering::Relaxed) > spills);
    assert_eq!(
        rows(&db, "SELECT count(*), max(s), sum(length(s)) FROM big"),
        want
    );
    db.close().unwrap();
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "the scratch file is gone"
    );
    let db = open(&path, Some(Credential::Key([2; 32]))).unwrap();
    assert_eq!(
        rows(&db, "SELECT count(*), max(s), sum(length(s)) FROM big"),
        want
    );
}

/// Backups of an encrypted database (page-service.md §9, encryption.md §8): the page-identical copy
/// keeps the ciphertext pages at their indexes and the key block, under sealed bootstrap metas; the
/// compacted copy reseals every page under the same data key. Both open with the credential.
#[test]
fn an_encrypted_database_backs_up_in_both_forms() {
    let path = tmp("enc_backup_src.jed");
    let db = create(&path, Credential::Key(KEY));
    populate(&db);
    let before = summary(&db);
    let source = std::fs::read(&path).unwrap();
    for (form, page_identical) in [("pages", true), ("compact", false)] {
        let dest = tmp(&format!("enc_backup_{form}.jed"));
        db.backup_for_testing(&dest, page_identical).unwrap();
        let copy = std::fs::read(&dest).unwrap();
        assert!(!copy.windows(7).any(|w| w == b"secret-"), "{form}");
        let ps = PAGE as usize;
        assert_eq!(
            copy[2 * ps..4 * ps],
            source[2 * ps..4 * ps],
            "{form}: key block"
        );
        if page_identical {
            // Every page the copy holds past the metas is the source's, byte for byte.
            for i in 4..copy.len() / ps {
                let page = &copy[i * ps..(i + 1) * ps];
                assert!(page.iter().all(|&b| b == 0) || page == &source[i * ps..(i + 1) * ps]);
            }
        }
        assert_eq!(open(&dest, None).err().unwrap().code(), "28P02", "{form}");
        let copy = open(&dest, Some(Credential::Key(KEY))).unwrap();
        assert_eq!(summary(&copy), before, "{form}");
        exec(
            &copy,
            "INSERT INTO t VALUES (9000, 'in the copy', NULL, NULL)",
        );
        copy.close().unwrap();
    }
    // After a credential change the copy carries the new key block.
    db.change_credential("main", Credential::Key([5; 32]), Argon2Params::default())
        .unwrap();
    let dest = tmp("enc_backup_changed.jed");
    db.backup_for_testing(&dest, true).unwrap();
    assert_eq!(
        open(&dest, Some(Credential::Key(KEY)))
            .err()
            .unwrap()
            .code(),
        "28P01"
    );
    assert_eq!(
        summary(&open(&dest, Some(Credential::Key([5; 32]))).unwrap()),
        before
    );
}
