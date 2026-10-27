//! The integrity check — `Database::integrity_check` (spec/design/integrity-check.md). It is host
//! API, so its contract lives here, not in the SQL corpus: every damaged fixture of
//! spec/fileformat/integrity/ reproduces its expected report exactly (everything but `detail`), every
//! golden that opens checks clean at both levels, and the host surface (`42704`, cancellation,
//! `max_findings`, a writer open beside the check, findings only a live handle can have) behaves as
//! §2 and §5 say. Mirrors impl/go/integrity_check_test.go and impl/ts/tests/integrity_check.test.ts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jed::integrity_kinds as kinds;
use jed::{
    CancellationToken, CreateOptions, Credential, Database, IntegrityCheckOptions, IntegrityLevel,
    IntegrityReport, OpenOptions, SessionOptions,
};

fn spec(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec")
        .join(rel)
}

/// A path under Cargo's per-test temp dir, cleared of any earlier run.
fn tmp(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = std::fs::remove_dir_all(PathBuf::from(lock));
    path
}

/// Open a copy of `image` read-only, as `jed check` would.
fn open_copy(name: &str, image: &[u8], credential: Option<Credential>) -> jed::Result<Database> {
    let path = tmp(&format!("integrity_{name}"));
    std::fs::write(&path, image).unwrap();
    Database::open_with_options(
        &path,
        OpenOptions {
            read_only: true,
            credential,
            environment: super::fileformat_golden::golden_env(),
            ..OpenOptions::default()
        },
    )
}

fn opts(level: IntegrityLevel, max_findings: u32) -> IntegrityCheckOptions {
    IntegrityCheckOptions {
        level,
        max_findings,
        cancel: None,
    }
}

// --- a minimal JSON reader for the fixtures' expected reports -------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    fn get(&self, key: &str) -> &Json {
        match self {
            Json::Obj(m) => m.get(key).unwrap_or_else(|| panic!("no key {key}")),
            _ => panic!("not an object"),
        }
    }
    fn arr(&self) -> &[Json] {
        match self {
            Json::Arr(a) => a,
            _ => panic!("not an array"),
        }
    }
    fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            Json::Null => None,
            _ => panic!("not a string"),
        }
    }
    fn num(&self) -> Option<u64> {
        match self {
            Json::Num(n) => Some(*n as u64),
            Json::Null => None,
            _ => panic!("not a number"),
        }
    }
    fn bool(&self) -> bool {
        match self {
            Json::Bool(b) => *b,
            _ => panic!("not a boolean"),
        }
    }
}

fn parse_json(text: &str) -> Json {
    fn ws(b: &[u8], i: &mut usize) {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
    }
    fn value(b: &[u8], i: &mut usize) -> Json {
        ws(b, i);
        match b[*i] {
            b'{' => {
                *i += 1;
                let mut m = BTreeMap::new();
                loop {
                    ws(b, i);
                    if b[*i] == b'}' {
                        *i += 1;
                        return Json::Obj(m);
                    }
                    let Json::Str(k) = value(b, i) else {
                        panic!("object key")
                    };
                    ws(b, i);
                    assert_eq!(b[*i], b':');
                    *i += 1;
                    let v = value(b, i);
                    m.insert(k, v);
                    ws(b, i);
                    if b[*i] == b',' {
                        *i += 1;
                    }
                }
            }
            b'[' => {
                *i += 1;
                let mut a = Vec::new();
                loop {
                    ws(b, i);
                    if b[*i] == b']' {
                        *i += 1;
                        return Json::Arr(a);
                    }
                    a.push(value(b, i));
                    ws(b, i);
                    if b[*i] == b',' {
                        *i += 1;
                    }
                }
            }
            b'"' => {
                *i += 1;
                let mut s = String::new();
                while b[*i] != b'"' {
                    assert_ne!(b[*i], b'\\', "escapes are not expected in the fixtures");
                    s.push(b[*i] as char);
                    *i += 1;
                }
                *i += 1;
                Json::Str(s)
            }
            b't' => {
                *i += 4;
                Json::Bool(true)
            }
            b'f' => {
                *i += 5;
                Json::Bool(false)
            }
            b'n' => {
                *i += 4;
                Json::Null
            }
            _ => {
                let start = *i;
                while *i < b.len() && (b[*i] == b'-' || b[*i].is_ascii_digit() || b[*i] == b'.') {
                    *i += 1;
                }
                Json::Num(std::str::from_utf8(&b[start..*i]).unwrap().parse().unwrap())
            }
        }
    }
    value(text.as_bytes(), &mut 0)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The contract part of a report (everything but `detail`), in the fixtures' shape.
fn contract(r: &IntegrityReport) -> String {
    let findings: Vec<String> = r
        .findings
        .iter()
        .map(|f| {
            format!(
                "{} {} {:?} {:?} {:?}",
                f.kind,
                f.database,
                f.object,
                f.page,
                f.key.as_deref().map(hex)
            )
        })
        .collect();
    let skipped: Vec<String> = r
        .skipped
        .iter()
        .map(|s| format!("{} {}", s.object, s.reason))
        .collect();
    format!(
        "findings {findings:#?}\ntruncated {}\nskipped {skipped:?}\npages {}\nrows {}",
        r.truncated, r.pages, r.rows
    )
}

fn expected(j: &Json) -> String {
    let findings: Vec<String> = j
        .get("findings")
        .arr()
        .iter()
        .map(|f| {
            format!(
                "{} {} {:?} {:?} {:?}",
                f.get("kind").str().unwrap(),
                f.get("database").str().unwrap(),
                f.get("object").str().map(str::to_string),
                f.get("page").num().map(|p| p as u32),
                f.get("key").str().map(str::to_string)
            )
        })
        .collect();
    let skipped: Vec<String> = j
        .get("skipped")
        .arr()
        .iter()
        .map(|s| {
            format!(
                "{} {}",
                s.get("object").str().unwrap(),
                s.get("reason").str().unwrap()
            )
        })
        .collect();
    format!(
        "findings {findings:#?}\ntruncated {}\nskipped {skipped:?}\npages {}\nrows {}",
        j.get("truncated").bool(),
        j.get("pages").num().unwrap(),
        j.get("rows").num().unwrap()
    )
}

/// Every damaged fixture reproduces each of its expected reports on a freshly opened file (in memory
/// the live count and the persisted free list do not apply, integrity-check.md §2).
#[test]
fn damaged_fixtures_reproduce_their_reports() {
    let dir = spec("fileformat/integrity");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.strip_suffix(".json").map(str::to_string)
        })
        .collect();
    names.sort();
    assert!(names.len() >= 10, "the integrity fixtures are present");
    for name in names {
        let image = std::fs::read(dir.join(format!("{name}.jed"))).unwrap();
        let want = parse_json(&std::fs::read_to_string(dir.join(format!("{name}.json"))).unwrap());
        let file = open_copy(&format!("fixture_{name}.jed"), &image, None)
            .unwrap_or_else(|e| panic!("{name}.jed does not open: {e}"));
        for run in want.get("runs").arr() {
            let level = match run.get("level").str().unwrap() {
                "quick" => IntegrityLevel::Quick,
                "full" => IntegrityLevel::Full,
                other => panic!("level {other}"),
            };
            let max = run.get("max_findings").num().unwrap() as u32;
            let got = file
                .integrity_check("main", &opts(level, max))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                contract(&got),
                expected(run.get("report")),
                "{name}.jed {level:?} max {max}"
            );
            assert_eq!(got.ok(), got.findings.is_empty() && !got.truncated);
        }
    }
}

/// The one non-malformed golden `open` rejects (validated_cow_tests.rs: a complete manifest whose
/// free list frees its own dependencies); every other golden opens and checks clean, including the
/// torn `cow_*` images, through recovery's fallback generation.
const UNOPENABLE: &[&str] = &["cow_invalid_free_list.jed"];

/// Every golden that opens checks with no findings at both levels, file-backed and (plaintext) in
/// memory — except `collation_skew_corrupt.jed`, whose index is deliberately ordered for another
/// collation version (integrity-check.md §6).
#[test]
fn clean_goldens_check_clean() {
    let dir = spec("fileformat/fixtures");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".jed"))
        .collect();
    names.sort();
    let mut unopenable = Vec::new();
    let mut checked = 0;
    for name in &names {
        // The malformed goldens are damaged on purpose (each one's damage is what open or a read
        // must reject); the skewed index is ordered for another collation version.
        if name == "collation_skew_corrupt.jed" || name.starts_with("malformed_") {
            continue;
        }
        let image = std::fs::read(dir.join(name)).unwrap();
        let credential = name
            .starts_with("encrypted_")
            .then_some(Credential::Key([0x4A; 32]));
        let encrypted = credential.is_some();
        let db = match open_copy(&format!("golden_{name}"), &image, credential) {
            Ok(db) => db,
            Err(_) => {
                unopenable.push(name.as_str());
                continue;
            }
        };
        let mut backings = vec![("file", db)];
        if !encrypted
            && let Ok(memory) = Database::from_image(&image, super::fileformat_golden::golden_env())
        {
            backings.push(("memory", memory));
        }
        for (backing, db) in &backings {
            for level in [IntegrityLevel::Quick, IntegrityLevel::Full] {
                let report = db
                    .integrity_check("main", &opts(level, 0))
                    .unwrap_or_else(|e| panic!("{name} ({backing}): {e}"));
                assert!(
                    report.ok(),
                    "{name} {level:?} ({backing}): {:#?}",
                    report.findings
                );
            }
        }
        checked += 1;
    }
    assert_eq!(unopenable, UNOPENABLE, "the goldens that do not open");
    assert!(checked > 60, "checked {checked} goldens");
}

/// A skewed index is the one golden with findings: its entries are ordered for another collation,
/// so `full` misses them.
#[test]
fn collation_skew_index_is_found() {
    let image = std::fs::read(spec("fileformat/fixtures/collation_skew_corrupt.jed")).unwrap();
    let Ok(db) = open_copy("skew.jed", &image, None) else {
        return; // the golden tests decide whether it opens
    };
    let report = db
        .integrity_check("main", &IntegrityCheckOptions::default())
        .unwrap();
    assert!(!report.ok());
}

fn create_file(path: &Path) -> Database {
    Database::create(CreateOptions {
        path: Some(path.to_path_buf()),
        page_size: 256,
        skip_fsync: true,
        ..Default::default()
    })
    .expect("create file db")
}

fn run(db: &Database, sql: &str) {
    db.session(SessionOptions::default())
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {}", e.message));
}

fn populate(db: &Database) {
    run(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32, note text)");
    run(db, "CREATE INDEX t_v ON t (v)");
    run(
        db,
        "INSERT INTO t SELECT g, g % 7, repeat('n', g % 300) FROM generate_series(1, 200) g",
    );
    run(db, "CREATE SEQUENCE s");
}

#[test]
fn unknown_database_is_42704() {
    let db = Database::default();
    for name in ["nope", "temp"] {
        let e = db
            .integrity_check(name, &IntegrityCheckOptions::default())
            .unwrap_err();
        assert_eq!(e.code(), "42704", "{name}");
    }
}

#[test]
fn cancellation_is_57014() {
    let db = Database::default();
    populate(&db);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let e = db
        .integrity_check(
            "main",
            &IntegrityCheckOptions {
                cancel: Some(cancel),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert_eq!(e.code(), "57014");
}

/// `max_findings` stops at the limit and sets `truncated`; the report is a prefix of the full one.
#[test]
fn max_findings_truncates() {
    let image = std::fs::read(spec("fileformat/integrity/leaf_keys_swapped.jed")).unwrap();
    let db = open_copy("truncate.jed", &image, None).unwrap();
    let all = db
        .integrity_check("main", &opts(IntegrityLevel::Full, 0))
        .unwrap();
    assert!(all.findings.len() > 1 && !all.truncated);
    let one = db
        .integrity_check("main", &opts(IntegrityLevel::Full, 1))
        .unwrap();
    assert!(one.truncated && !one.ok());
    assert_eq!(one.findings, all.findings[..1]);
}

/// In-memory and file databases check clean, and an attachment checks by its name.
#[test]
fn clean_databases_and_attachments() {
    let memory = Database::default();
    populate(&memory);
    let r = memory
        .integrity_check("main", &IntegrityCheckOptions::default())
        .unwrap();
    assert!(r.ok(), "{:#?}", r.findings);
    assert_eq!(r.rows, 200);

    let path = tmp("integrity_clean.jed");
    let file = create_file(&path);
    populate(&file);
    run(&file, "DELETE FROM t WHERE id % 3 = 0");
    let r = file
        .integrity_check("main", &IntegrityCheckOptions::default())
        .unwrap();
    assert!(r.ok(), "{:#?}", r.findings);
    assert_eq!(r.rows, 134);

    let other = tmp("integrity_attached.jed");
    let other_db = create_file(&other);
    populate(&other_db);
    other_db.close().unwrap();
    file.attach("other", jed::AttachSource::file(&other), false)
        .unwrap();
    let r = file
        .integrity_check("other", &IntegrityCheckOptions::default())
        .unwrap();
    assert!(r.ok(), "{:#?}", r.findings);
    assert_eq!(r.rows, 200);
}

/// The check pins the committed snapshot: a write transaction open on the same handle neither
/// blocks it nor shows through.
#[test]
fn runs_beside_an_open_writer() {
    let path = tmp("integrity_writer.jed");
    let db = create_file(&path);
    populate(&db);
    let mut writer = db.session(SessionOptions::default());
    writer.begin(true).unwrap();
    writer
        .execute(
            "INSERT INTO t SELECT g, g, 'w' FROM generate_series(1000, 1100) g",
            &[],
        )
        .unwrap();
    let r = db
        .integrity_check("main", &IntegrityCheckOptions::default())
        .unwrap();
    assert!(r.ok(), "{:#?}", r.findings);
    assert_eq!(r.rows, 200);
    writer.commit().unwrap();
    let r = db
        .integrity_check("main", &IntegrityCheckOptions::default())
        .unwrap();
    assert!(r.ok(), "{:#?}", r.findings);
    assert_eq!(r.rows, 301);
}

fn crc32(data: impl Iterator<Item = u8>) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// The selected meta slot of a 256-byte-page file at version `txid`, and its catalog root.
fn meta_slot(image: &[u8], txid: u64) -> (usize, u32) {
    for slot in 0..2 {
        let m = &image[slot * 256..(slot + 1) * 256];
        if u64::from_be_bytes(m[12..20].try_into().unwrap()) == txid {
            return (slot, u32::from_be_bytes(m[20..24].try_into().unwrap()));
        }
    }
    panic!("no meta slot holds {txid}");
}

/// Damage written under an open handle after it opened: only a live handle can see it, because a
/// fresh open would fail or select the other slot (the registry's `fresh = false` kinds).
#[test]
fn live_only_findings() {
    let path = tmp("integrity_live.jed");
    let db = create_file(&path);
    populate(&db);
    let clean = db
        .integrity_check("main", &IntegrityCheckOptions::default())
        .unwrap();
    assert!(clean.ok(), "{:#?}", clean.findings);
    let txid = db.txid();
    let original = std::fs::read(&path).unwrap();
    let (slot, catalog) = meta_slot(&original, txid);
    let kinds_of = |r: &IntegrityReport| r.findings.iter().map(|f| f.kind).collect::<Vec<_>>();

    // A catalog page whose checksum no longer verifies: the manifest lists it (the last commit
    // rewrote the catalog), and the catalog re-read finds it.
    let mut image = original.clone();
    image[catalog as usize * 256 + 200] ^= 0x01;
    std::fs::write(&path, &image).unwrap();
    let r = db
        .integrity_check("main", &IntegrityCheckOptions::default())
        .unwrap();
    assert_eq!(
        kinds_of(&r),
        [kinds::MANIFEST_PAGE_MISMATCH, kinds::CATALOG_PAGE_CORRUPT]
    );
    assert!(r.findings.iter().all(|f| f.page == Some(catalog)));

    // The same catalog page with a valid checksum but other content: it no longer encodes the
    // snapshot's catalog.
    let mut image = original.clone();
    let at = catalog as usize * 256;
    let payload_end = image[at + 16..at + 256]
        .iter()
        .rposition(|b| *b != 0)
        .expect("a catalog page has content")
        + 16;
    image[at + payload_end] ^= 0x01; // a byte inside the last entry
    let crc = crc32(
        image[at..at + 12]
            .iter()
            .chain(&image[at + 16..at + 256])
            .copied(),
    );
    image[at + 12..at + 16].copy_from_slice(&crc.to_be_bytes());
    std::fs::write(&path, &image).unwrap();
    let r = db
        .integrity_check("main", &IntegrityCheckOptions::default())
        .unwrap();
    assert_eq!(
        kinds_of(&r),
        [kinds::MANIFEST_PAGE_MISMATCH, kinds::CATALOG_MISMATCH],
        "{:#?}",
        r.findings
    );

    // The pinned generation's meta slot no longer verifies.
    let mut image = original.clone();
    image[slot * 256 + 100] ^= 0x01;
    std::fs::write(&path, &image).unwrap();
    let r = db
        .integrity_check("main", &IntegrityCheckOptions::default())
        .unwrap();
    assert_eq!(kinds_of(&r), [kinds::META_INVALID]);
    assert_eq!(r.findings[0].page, Some(slot as u32));

    std::fs::write(&path, &original).unwrap();
    let r = db
        .integrity_check("main", &IntegrityCheckOptions::default())
        .unwrap();
    assert!(r.ok(), "{:#?}", r.findings);
}
