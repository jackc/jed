//! Resealed structural corruption (storage.md §7): the per-page CRC proves a page was written whole,
//! not that it is the page its referrer expected or that its directories are self-consistent. A page
//! copied from another table, or a CRC-valid page whose in-page lengths and offsets disagree with each
//! other, reaches the decoders. They must fail closed with a structured error — never panic, abort,
//! hang, or index past a slice. The checksum tests cannot reach these paths: their byte corruptions
//! all break a CRC first.
//!
//! Each case mutates one body page of a golden fixture, recomputes that page's CRC, then opens the
//! image and scans and deletes every table. Wrong rows are possible (a whole-page transposition is
//! undetectable), so the oracle is only "engine error or success". Mirrors
//! impl/go/resealed_corruption_test.go (same seeds, same mutation operators).

use jed::{Database, SessionOptions};
use std::path::PathBuf;
use std::sync::{Mutex, Once, mpsc};
use std::time::Duration;

const PAGE_HEADER: usize = 16;

/// The number of seeded mutations per body page. `JED_RESEALED_MUTATIONS` raises it for an
/// exploration run.
fn resealed_mutations() -> usize {
    std::env::var("JED_RESEALED_MUTATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(6)
}

/// splitmix64, so a failing case is reproducible from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn intn(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// The directory values most likely to land on an unchecked bound.
    fn interesting_u32(&mut self, ps: usize, pages: usize) -> u32 {
        match self.intn(11) {
            9 | 10 => (2 + self.intn(pages - 2)) as u32, // a misdirected but readable pointer
            0 => 0,
            1 => 1,
            2 => ps as u32,
            3 => (ps - PAGE_HEADER) as u32,
            4 => (pages + self.intn(4)) as u32,
            5 => 0xFFFF_FFFF,
            6 => 0x7FFF_FFFF,
            7 => self.intn(64) as u32,
            _ => self.next() as u32,
        }
    }
}

/// Apply one seeded structural mutation to page `p` of `image` and reseal its CRC.
fn mutate_page(image: &mut [u8], ps: usize, p: usize, pages: usize, r: &mut Rng) -> String {
    let what = match r.intn(8) {
        0 => {
            // one byte anywhere outside the CRC field
            let mut off = r.intn(ps - 4);
            if off >= 12 {
                off += 4;
            }
            image[p * ps + off] = r.next() as u8;
            format!("byte {off}")
        }
        1 => {
            // a u32 field in the payload, aligned or not
            let off = PAGE_HEADER + r.intn(ps - PAGE_HEADER - 4);
            let v = r.interesting_u32(ps, pages);
            image[p * ps + off..p * ps + off + 4].copy_from_slice(&v.to_be_bytes());
            format!("u32 at {off}")
        }
        2 => {
            // a u16 field in the payload
            let off = PAGE_HEADER + r.intn(ps - PAGE_HEADER - 2);
            let v = r.interesting_u32(ps, pages) as u16;
            image[p * ps + off..p * ps + off + 2].copy_from_slice(&v.to_be_bytes());
            format!("u16 at {off}")
        }
        3 => {
            let v = r.interesting_u32(ps, pages);
            image[p * ps + 4..p * ps + 8].copy_from_slice(&v.to_be_bytes());
            "item count".to_string()
        }
        4 => {
            let v = r.interesting_u32(ps, pages);
            image[p * ps + 8..p * ps + 12].copy_from_slice(&v.to_be_bytes());
            "next page".to_string()
        }
        5 => {
            image[p * ps] = r.intn(10) as u8;
            "page type".to_string()
        }
        6 => {
            // a whole other body page, copied with its own (valid) CRC
            let src = 2 + r.intn(pages - 2);
            image.copy_within(src * ps..(src + 1) * ps, p * ps);
            return format!("copy of page {src}");
        }
        _ => {
            for _ in 0..2 + r.intn(6) {
                let off = PAGE_HEADER + r.intn(ps - PAGE_HEADER);
                image[p * ps + off] = r.next() as u8;
            }
            "byte burst".to_string()
        }
    };
    let crc = jed::format::page_crc(&image[p * ps..(p + 1) * ps]);
    image[p * ps + 12..p * ps + 16].copy_from_slice(&crc.to_be_bytes());
    what
}

/// Cut an image to its committed page high-water (max page count over the valid meta slots), dropping
/// trailing preallocation slack; the loader reads no page past it.
fn trim_to_high_water(image: &[u8]) -> &[u8] {
    if image.len() < 36 {
        return image;
    }
    let ps = u32::from_be_bytes(image[8..12].try_into().unwrap()) as usize;
    if ps < 36 || image.len() < 2 * ps {
        return image;
    }
    let mut high = 0;
    for slot in 0..2 {
        let base = slot * ps;
        if u16::from_be_bytes([image[base + 4], image[base + 5]]) != jed::format::FORMAT_VERSION {
            continue;
        }
        let pc = u32::from_be_bytes(image[base + 24..base + 28].try_into().unwrap()) as usize;
        high = high.max(pc);
    }
    if high == 0 || high * ps > image.len() {
        return image;
    }
    &image[..high * ps]
}

/// The statements run against every table. The UPDATE rewrites every row through its indexes; the
/// DELETE materializes each leaf through the mutation path and visits every index. Each commits.
fn resealed_statements(db: &Database) -> Vec<String> {
    let quote = |n: &str| format!("\"{}\"", n.replace('"', "\"\""));
    let mut out = Vec::new();
    for name in db.table_names() {
        let q = quote(&name);
        out.push(format!("SELECT * FROM {q}"));
        out.push(format!("SELECT count(*) FROM {q}"));
        let table = db.table(&name).expect("a listed table");
        if let Some(c) = table.columns.iter().find(|c| !c.primary_key) {
            let c = quote(&c.name);
            out.push(format!("UPDATE {q} SET {c} = {c}"));
        }
        out.push(format!("DELETE FROM {q}"));
    }
    out
}

/// Open `image` and run `stmts`, rendering every cell. Engine errors are the acceptable failure.
fn run_case(image: &[u8], stmts: &[String]) {
    let Ok(db) = Database::from_image(image) else {
        return;
    };
    let mut s = db.session(SessionOptions::default());
    for sql in stmts {
        if let Ok(mut rows) = s.query(sql, &[]) {
            for row in rows.by_ref() {
                for v in &row {
                    let _ = v.render();
                }
            }
            let _ = rows.error();
        }
    }
}

const CASE_THREAD: &str = "resealed-case";

/// The last panic raised on a case thread (message, location, and the engine frames below it).
static PANIC: Mutex<Option<String>> = Mutex::new(None);

/// Route case-thread panics into `PANIC` instead of stderr; every other thread keeps the previous
/// hook, so concurrently running tests report normally.
fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if std::thread::current().name() != Some(CASE_THREAD) {
                return prev(info);
            }
            let bt = std::backtrace::Backtrace::force_capture().to_string();
            let frames: Vec<&str> = bt
                .lines()
                .map(str::trim)
                .filter(|l| l.contains("jed::") && !l.contains("resealed_corruption"))
                .take(6)
                .collect();
            *PANIC.lock().unwrap() = Some(format!("{info} [{}]", frames.join(" < ")));
        }));
    });
}

/// Run one case on its own thread: a panic or a hang becomes an `Err` naming it.
fn guarded(image: Vec<u8>, stmts: Vec<String>) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name(CASE_THREAD.to_string())
        .stack_size(8 << 20)
        .spawn(move || {
            run_case(&image, &stmts);
            let _ = tx.send(());
        })
        .expect("spawn case thread");
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(()) => Ok(()),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err("open+scan did not return within 10s (possible infinite loop)".to_string())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(format!(
            "panic: {}",
            PANIC
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| "unknown".to_string())
        )),
    }
}

#[test]
fn resealed_page_corruption_fails_closed() {
    let bundle = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec/collation/fixtures/unicode.jucd");
    jed::load_unicode_data(&std::fs::read(&bundle).unwrap()).expect("load unicode.jucd");
    install_panic_hook();

    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/fileformat/fixtures");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "jed"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no fixtures in {}", dir.display());
    let per = resealed_mutations();
    let (mut cases, mut skipped) = (0usize, 0usize);
    let mut failures = Vec::new();
    for path in &paths {
        let base = path.file_name().unwrap().to_str().unwrap().to_string();
        let raw = std::fs::read(path).unwrap();
        let clean = match Database::from_image(&raw) {
            Ok(db) => db,
            Err(_) => {
                skipped += 1; // a fixture that is malformed by design
                continue;
            }
        };
        let stmts = resealed_statements(&clean);
        drop(clean);

        let image = trim_to_high_water(&raw);
        let ps = u32::from_be_bytes(image[8..12].try_into().unwrap()) as usize;
        let pages = image.len() / ps;
        for p in 2..pages {
            for m in 0..per {
                let seed = ((p as u64) << 32) | m as u64;
                let mut r = Rng(seed ^ ((base.len() as u64) << 48));
                let mut mutated = image.to_vec();
                let what = mutate_page(&mut mutated, ps, p, pages, &mut r);
                cases += 1;
                if let Err(e) = guarded(mutated, stmts.clone()) {
                    failures.push(format!("{base} page {p} mutation {m} ({what}): {e}"));
                }
            }
        }
    }
    eprintln!("{cases} resealed cases, {skipped} fixtures skipped");
    assert!(
        failures.is_empty(),
        "{} of {cases} cases failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
