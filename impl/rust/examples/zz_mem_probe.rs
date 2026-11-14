//! EXPERIMENT (not part of the core's test contract): live heap held by a file database after its
//! tables have been read, against the bytes of the pages read. Skipped unless `JED_MEM_PROBE` names a
//! database file (the Go experiment's `sb.jed`).
//!
//!     JED_MEM_PROBE=/path/sb.jed cargo run --release --example zz_mem_probe

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use jed::Value;
use jed::{Database, OpenOptions, SessionOptions};

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(
            new_size as isize - layout.size() as isize,
            Ordering::Relaxed,
        );
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live_mb() -> f64 {
    LIVE.load(Ordering::Relaxed) as f64 / (1 << 20) as f64
}

fn main() {
    let Ok(path) = std::env::var("JED_MEM_PROBE") else {
        return;
    };
    println!("size_of::<Value>() = {}", std::mem::size_of::<Value>());
    let file_mb = std::fs::metadata(&path).unwrap().len() as f64 / (1 << 20) as f64;
    let base = live_mb();
    let db = Database::open_with_options(
        &path,
        OpenOptions {
            read_only: true,
            cache_bytes: 1 << 30,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let mut s = db.session(SessionOptions::default());
    println!("file {file_mb:.1} MB");
    println!("live heap after open: {:.1} MB", live_mb() - base);
    let tables = [
        "orders",
        "line_items",
        "events",
        "users",
        "sessions",
        "activity",
        "samples",
    ];
    // One aggregate per table over its last column only (the columnar lane where it applies).
    for t in tables {
        let mut rows = 0u64;
        for r in s
            .query(&format!("SELECT count(*) FROM {t} WHERE true"), &[])
            .unwrap()
        {
            r.unwrap();
            rows += 1;
        }
        assert_eq!(rows, 1);
    }
    println!(
        "live heap after count(*) of every table: {:.1} MB",
        live_mb() - base
    );
    // Every column of every row, streamed and dropped.
    let mut total = 0u64;
    for t in tables {
        for r in s.query(&format!("SELECT * FROM {t}"), &[]).unwrap() {
            r.unwrap();
            total += 1;
        }
    }
    println!(
        "live heap after SELECT * of every table ({total} rows): {:.1} MB",
        live_mb() - base
    );
    // Range scans through each secondary index.
    for q in [
        "SELECT count(*) FROM orders WHERE customer_id >= 0",
        "SELECT count(*) FROM orders WHERE status >= ''",
        "SELECT count(*) FROM line_items WHERE product_id >= 0",
        "SELECT count(*) FROM events WHERE device_id >= 0",
        "SELECT count(*) FROM users WHERE email >= ''",
    ] {
        for r in s.query(q, &[]).unwrap() {
            r.unwrap();
        }
    }
    println!(
        "live heap after index range scans: {:.1} MB",
        live_mb() - base
    );
    drop(s);
    drop(db);
    println!("live heap after close: {:.1} MB", live_mb() - base);
}
