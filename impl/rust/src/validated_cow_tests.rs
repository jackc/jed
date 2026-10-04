//! Persistence-order tests against the production writer, with separate cache and durable bytes.
use crate::blockstore::BlockStore;
use crate::pager::Pager;
use crate::{Engine, EngineError, Result, SqlState, Value, execute};
use std::sync::{Arc, Mutex};

const PS: usize = 256;

#[test]
fn complete_manifest_with_self_freeing_dependencies_is_corrupt() {
    let image = include_bytes!("../../../spec/fileformat/fixtures/cow_invalid_free_list.jed");
    // The newest manifest is complete. Logical corruption must fail after selection,
    // rather than silently returning the otherwise valid preceding generation.
    assert!(matches!(Engine::from_image(image), Err(e) if e.code() == "XX001"));
}

#[derive(Default)]
struct Device {
    cache: Vec<u8>,
    durable: Vec<u8>,
    writes: Vec<(usize, Vec<u8>)>,
    syncs: usize,
    fail_sync: Option<usize>,
    fail_read: Option<u32>,
}
struct Store(Arc<Mutex<Device>>);
fn io_failure() -> EngineError {
    EngineError::new(SqlState::IoError, "injected device failure")
}
impl BlockStore for Store {
    fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let d = self.0.lock().unwrap();
        if d.fail_read == Some((offset as usize / PS) as u32) {
            return Err(io_failure());
        }
        d.cache
            .get(offset as usize..offset as usize + len)
            .map(|b| b.to_vec())
            .ok_or_else(io_failure)
    }
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        let mut d = self.0.lock().unwrap();
        let offset = offset as usize;
        d.cache[offset..offset + bytes.len()].copy_from_slice(bytes);
        d.writes.push((offset, bytes.to_vec()));
        Ok(())
    }
    fn sync(&mut self) -> Result<()> {
        let mut d = self.0.lock().unwrap();
        d.syncs += 1;
        if d.fail_sync == Some(d.syncs) {
            return Err(io_failure());
        }
        d.durable = d.cache.clone();
        Ok(())
    }
    fn size(&mut self) -> Result<u64> {
        Ok(self.0.lock().unwrap().cache.len() as u64)
    }
    fn set_size(&mut self, size: u64) -> Result<()> {
        let mut d = self.0.lock().unwrap();
        d.cache.resize(size as usize, 0);
        d.durable = d.cache.clone();
        Ok(())
    }
}
fn open(device: &Arc<Mutex<Device>>) -> Engine {
    let mut db = Engine::open_paged(
        Pager::from_store(Box::new(Store(device.clone()))).unwrap(),
        1024,
    )
    .unwrap();
    db.path = Some("fault-device".into());
    db
}
fn seeded(rows: usize) -> (Arc<Mutex<Device>>, Engine) {
    let mut seed = Engine::new();
    seed.page_size = PS as u32;
    execute(&mut seed, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)").unwrap();
    let tuples = (0..rows)
        .map(|id| format!("({id},0)"))
        .collect::<Vec<_>>()
        .join(",");
    execute(&mut seed, &format!("INSERT INTO t VALUES {tuples}")).unwrap();
    let mut initial = seed.to_image(PS as u32, 1).unwrap();
    initial.resize(1024 * PS, 0); // no allocation barriers during the measured commit
    let device = Arc::new(Mutex::new(Device {
        cache: initial.clone(),
        durable: initial,
        ..Default::default()
    }));
    let db = open(&device);
    (device, db)
}
fn values(db: &Engine) -> Vec<i64> {
    db.rows_in_key_order("t")
        .unwrap()
        .iter()
        .map(|row| match row[1] {
            Value::Int(n) => n,
            _ => panic!("integer"),
        })
        .collect()
}
fn assert_snapshot(image: &[u8], rows: usize) {
    let db = Engine::from_image(image).unwrap();
    let values = values(&db);
    assert!(
        values == vec![0; rows] || values == vec![1; rows],
        "mixed or corrupt snapshot"
    );
}
#[test]
fn all_small_write_subsets_and_each_large_manifest_tear_are_atomic() {
    for rows in [2, 400] {
        let (device, mut db) = seeded(rows);
        let initial = device.lock().unwrap().durable.clone();
        execute(&mut db, "UPDATE t SET v=1").unwrap();
        let d = device.lock().unwrap();
        assert_eq!(d.syncs, 1, "one steady-state durability barrier");
        let writes = d.writes.clone();
        let complete = d.cache.clone();
        drop(d);
        let (meta_at, meta) = writes.last().unwrap();
        assert!(*meta_at < 2 * PS);
        if rows == 2 {
            assert!(writes.len() < 10);
            for mask in 0..1usize << writes.len() {
                let mut image = initial.clone();
                for (i, (at, bytes)) in writes.iter().enumerate() {
                    if mask & (1 << i) != 0 {
                        image[*at..at + bytes.len()].copy_from_slice(bytes);
                    }
                }
                assert_snapshot(&image, rows);
            }
        } else {
            assert!(
                u32::from_be_bytes(meta[44..48].try_into().unwrap()) > 1,
                "exercise multiple overflow pages"
            );
        }
        // Every body/manifest/meta write can be absent or torn independently, including when
        // the newest meta reached disk before all its dependencies.
        for (at, bytes) in &writes {
            for prefix in [0, 1, 31, PS / 2, PS - 1] {
                let mut image = complete.clone();
                image[*at..at + bytes.len()].copy_from_slice(&initial[*at..at + bytes.len()]);
                image[*at..at + prefix].copy_from_slice(&bytes[..prefix]);
                assert_snapshot(&image, rows);
            }
        }
    }
}
#[test]
fn recovered_cache_generation_is_stabilized_before_a_second_failed_commit() {
    let (device, mut db) = seeded(2);
    device.lock().unwrap().fail_sync = Some(1);
    assert_eq!(
        execute(&mut db, "UPDATE t SET v=1").unwrap_err().code(),
        "58030"
    );
    drop(db); // process crash: unsynced but complete cache survives
    let mut recovered = open(&device);
    assert_eq!(values(&recovered), vec![1, 1]);
    device.lock().unwrap().fail_sync = Some(3); // sync2 stabilizes adopted root; sync3 fails new root
    assert_eq!(
        execute(&mut recovered, "UPDATE t SET v=2")
            .unwrap_err()
            .code(),
        "58030"
    );
    let d = device.lock().unwrap();
    assert_eq!(d.syncs, 3);
    assert_eq!(values(&Engine::from_image(&d.durable).unwrap()), vec![1, 1]);
    let before = d.writes.len();
    drop(d);
    assert_eq!(
        execute(&mut recovered, "UPDATE t SET v=3")
            .unwrap_err()
            .code(),
        "58030"
    );
    assert_eq!(device.lock().unwrap().writes.len(), before);
}
#[test]
fn host_read_errors_do_not_silently_select_an_older_commit() {
    let (device, mut db) = seeded(2);
    execute(&mut db, "UPDATE t SET v=1").unwrap();
    let mut d = device.lock().unwrap();
    let first = d.writes[0].0 / PS;
    d.fail_read = Some(first as u32);
    drop(d);
    let pager = Pager::from_store(Box::new(Store(device))).unwrap();
    let result = Engine::open_paged(pager, 1024);
    assert!(matches!(result,Err(e) if e.code()=="58030"));
}

#[test]
fn serialization_error_leaves_writer_usable_without_storage_work() {
    for recovered in [false, true] {
        let (device, mut db) = seeded(2);
        execute(&mut db, "UPDATE t SET v=1").unwrap();
        if recovered {
            drop(db);
            db = open(&device);
        }
        let before = device.lock().unwrap().cache.clone();
        {
            let mut d = device.lock().unwrap();
            d.writes.clear();
            d.syncs = 0;
        }
        let columns = (0..40)
            .map(|i| format!("c{i} i32"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            execute(&mut db, &format!("CREATE TABLE wide ({columns})"))
                .unwrap_err()
                .code(),
            "0A000"
        );
        {
            let d = device.lock().unwrap();
            assert!(d.writes.is_empty());
            assert_eq!(d.syncs, 0);
            assert_eq!(d.cache, before);
        }
        execute(&mut db, "UPDATE t SET v=2").unwrap();
        assert_eq!(values(&db), vec![2, 2]);
        assert_eq!(device.lock().unwrap().syncs, if recovered { 2 } else { 1 });
    }
}

#[test]
fn page_limit_is_rejected_before_wrapping_into_metadata() {
    let seed = Engine::new();
    let result = seed
        .committed
        .incremental_image(PS as u32, u32::MAX, &[], true, None);
    assert!(matches!(result, Err(e) if e.code() == "54000"));
}
