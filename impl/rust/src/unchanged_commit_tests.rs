//! Public sessions over a counting byte device: the shared scenarios assert storage effects,
//! which sqllogictest cannot observe. No mocked commit or serializer.
use super::*;
use crate::blockstore::{BlockStore, MemoryBlockStore};
use crate::pager::Pager;

struct Counts {
    store: MemoryBlockStore,
    writes: usize,
    syncs: usize,
    grows: usize,
}
struct CountingStore(Arc<Mutex<Counts>>);
impl BlockStore for CountingStore {
    fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.0.lock().unwrap().store.read_at(offset, len)
    }
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        let mut c = self.0.lock().unwrap();
        c.writes += 1;
        c.store.write_at(offset, bytes)
    }
    fn sync(&mut self) -> Result<()> {
        self.0.lock().unwrap().syncs += 1;
        Ok(())
    }
    fn size(&mut self) -> Result<u64> {
        self.0.lock().unwrap().store.size()
    }
    fn set_size(&mut self, size: u64) -> Result<()> {
        let mut c = self.0.lock().unwrap();
        c.grows += 1;
        c.store.set_size(size)
    }
}
fn image(count: &Arc<Mutex<Counts>>) -> Vec<u8> {
    let mut c = count.lock().unwrap();
    let n = c.store.size().unwrap() as usize;
    c.store.read_at(0, n).unwrap()
}
#[test]
fn unchanged_commits_preserve_bytes_and_skip_barriers() {
    let bytes = Engine::new().to_image(4096, 1).unwrap();
    let count = Arc::new(Mutex::new(Counts {
        store: MemoryBlockStore::new(bytes),
        writes: 0,
        syncs: 0,
        grows: 0,
    }));
    let mut engine = Engine::open_paged(
        Pager::from_store(Box::new(CountingStore(count.clone()))).unwrap(),
        64,
    )
    .unwrap();
    engine.path = Some("counting-device".into());
    let db = Database::from_engine(engine);
    let mut s = db.session(SessionOptions::default());
    for sql in [
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
        "INSERT INTO t VALUES (1, 1)",
        "CREATE SEQUENCE s",
    ] {
        s.query_outcome(sql, &[]).unwrap();
    }
    for line in include_str!("../../../spec/conformance/storage/unchanged_commits.tsv").lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = line.splitn(3, '\t').collect();
        let (mode, delta, sql) = (fields[0], fields[1].parse::<u64>().unwrap(), fields[2]);
        let before = image(&count);
        let version = db.version();
        {
            let mut c = count.lock().unwrap();
            c.writes = 0;
            c.syncs = 0;
            c.grows = 0;
        }
        if mode != "auto" && mode != "script" {
            s.begin(true).unwrap();
        }
        let result = if mode == "script" {
            s.execute_script(sql).map(|_| ())
        } else {
            sql.split(';')
                .filter(|q| !q.trim().is_empty())
                .try_for_each(|q| s.query_outcome(q, &[]).map(|_| ()))
        };
        if mode == "failed" {
            assert!(result.is_err(), "{line}");
        } else {
            result.unwrap();
        }
        if mode == "tx" || mode == "failed" {
            s.commit().unwrap();
        }
        if mode == "rollback" {
            s.rollback().unwrap();
        }
        assert_eq!(db.version(), version + delta, "{line}");
        if delta == 0 {
            assert_eq!(image(&count), before, "{line}");
        }
        let c = count.lock().unwrap();
        if delta == 0 {
            assert_eq!((c.writes, c.syncs, c.grows), (0, 0, 0), "{line}");
        } else {
            assert!(c.writes > 0 && c.syncs > 0, "{line}");
        }
    }
}

#[test]
fn attachment_commit_leaves_main_unchanged() {
    let dir = std::env::temp_dir().join(format!("jed-noop-attach-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.jed");
    let attached = dir.join("attached.jed");
    drop(
        Database::create(CreateOptions {
            path: Some(attached.clone()),
            ..Default::default()
        })
        .unwrap(),
    );
    let db = Database::create(CreateOptions {
        path: Some(path.clone()),
        ..Default::default()
    })
    .unwrap();
    db.attach("a", AttachSource::file(&attached), false)
        .unwrap();
    let mut s = db.session(SessionOptions::default());
    s.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let version = db.version();
    s.begin(true).unwrap();
    for sql in [
        "DELETE FROM t WHERE id = 999",
        "CREATE TABLE a.t (id i32 PRIMARY KEY)",
        "INSERT INTO a.t VALUES (1)",
    ] {
        s.query_outcome(sql, &[]).unwrap();
    }
    s.commit().unwrap();
    assert_eq!(db.version(), version);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let before = std::fs::read(&attached).unwrap();
    s.query_outcome("DELETE FROM a.t WHERE id = 999", &[])
        .unwrap();
    assert_eq!(std::fs::read(&attached).unwrap(), before);
    drop(s);
    db.detach("a").unwrap();
    drop(db);
    let reopened = Database::open(&attached).unwrap();
    let mut s = reopened.session(SessionOptions::default());
    let out = s.query_outcome("SELECT id FROM t", &[]).unwrap();
    match out {
        Outcome::Query { rows, .. } => assert_eq!(rows.len(), 1),
        _ => panic!("query"),
    }
    drop(s);
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}
