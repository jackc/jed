//! Benchmark transport only: public APIs of the immutable Rust peer, no engine behavior.
use jed::{CreateOptions, OpenOptions, Database, PreparedStatement, Rows, Session, SessionOptions};
use std::{fs, path::Path, time::Instant};

struct Checksum { value: u64, cost: i64 }
impl Checksum {
    fn new() -> Self { Self { value: 14695981039346656037, cost: 0 } }
    fn text(&mut self, text: &str) {
        for byte in text.bytes() { self.value = (self.value ^ u64::from(byte)).wrapping_mul(1099511628211); }
    }
}
fn drain(session: &Session, mut rows: Rows, sum: &mut Checksum) -> u64 {
    let mut count = 0;
    for row in rows.by_ref() {
        for value in row.expect("drain row") { sum.text(&session.render(&value).expect("render value")); sum.text("\x1f"); }
        sum.text("\x1e"); count += 1;
    }
    sum.cost = sum.cost.checked_add(rows.cost()).expect("cost overflow");
    rows.close(); count
}
fn query(session: &mut Session, sql: &str, sum: &mut Checksum) -> u64 {
    let rows = session.query(sql, &[]).expect("query"); drain(session, rows, sum)
}
fn prepared(session: &mut Session, statement: &PreparedStatement, sum: &mut Checksum) -> u64 {
    let rows = session.query_prepared(statement, &[]).expect("prepared query"); drain(session, rows, sum)
}
fn parallel(database: &Database, sql: &str, sum: &mut Checksum) -> u64 {
    let mut workers=Vec::new();
    for _ in 0..4 {
        let db=database.clone(); let sql=sql.to_string();
        workers.push(std::thread::spawn(move || {
            let mut session=db.session(SessionOptions::default()); let statement=session.prepare(&sql).expect("prepare worker");
            let mut sum=Checksum::new(); let mut count=0;
            for _ in 0..64 {count+=prepared(&mut session,&statement,&mut sum);}
            session.close(); (sum,count)
        }));
    }
    let mut count=0;
    for worker in workers {let (part,n)=worker.join().expect("worker");sum.text(&format!("{:016x}",part.value));sum.text("\x1d");sum.cost+=part.cost;count+=n;}
    count
}
fn open(path: &Path) -> Database {Database::open_with_options(path,OpenOptions{cache_bytes:256*1024*1024,work_mem:256*1024*1024,..Default::default()}).expect("open")}
struct Workload { name: String, mode: String, warmup: usize, iterations: usize, sql: String }
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len()==2 && args[1]=="ready" {
        let db = Database::create(CreateOptions::default()).expect("create");
        let mut session = db.session(SessionOptions::default());
        query(&mut session,"SELECT 1",&mut Checksum::new());
        session.close(); drop(session); db.close().expect("close"); println!("ready"); return;
    }
    assert_eq!(args.len(),3,"INPUT DIRECTORY | ready");
    let mut setup = Vec::new(); let mut workloads = Vec::new();
    for line in fs::read_to_string(&args[1]).expect("input").lines() {
        let fields: Vec<&str> = line.splitn(6,'\t').collect();
        if fields.len()==2 && fields[0]=="setup" { setup.push(fields[1].to_string()); }
        else if fields.len()==6 && fields[0]=="case" {
            assert!(fields[1].bytes().all(|v| v.is_ascii_lowercase() || v.is_ascii_digit() || v==b'_'));
            assert!(["query","rollback","durable","cold_open","parallel"].contains(&fields[2]));
            let iterations = fields[4].parse().expect("iterations"); assert!(iterations>0);
            workloads.push(Workload{name:fields[1].to_string(),mode:fields[2].to_string(),warmup:fields[3].parse().expect("warmup"),iterations,sql:fields[5].to_string()});
        } else { panic!("invalid input") }
    }
    fs::create_dir_all(&args[2]).expect("directory");
    for w in workloads {
        let path = Path::new(&args[2]).join(format!("{}.jed",w.name));
        let db = Database::create(CreateOptions{path:Some(path.clone()),..Default::default()}).expect("create");
        let mut session = db.session(SessionOptions::default());
        for sql in &setup { query(&mut session,sql,&mut Checksum::new()); }
        session.close(); drop(session); db.close().expect("close setup");
        let db = if w.mode=="cold_open" {None} else {Some(open(&path))};
        let mut session = db.as_ref().map(|db| db.session(SessionOptions::default()));
        let statement = if w.mode=="parallel" {None} else {session.as_ref().map(|s| s.prepare(&w.sql).expect("prepare"))};
        let mut sum = Checksum::new(); let mut count = 0; let mut samples = Vec::new(); let mut warmups = Vec::new();
        for i in 0..w.warmup+w.iterations {
            let mut discarded = Checksum::new();
            let sink = if i<w.warmup {&mut discarded} else {&mut sum};
            let start = Instant::now();
            let rows = if w.mode=="cold_open" {
                let opened = open(&path);
                let mut reader = opened.session(SessionOptions::default());
                let stmt = reader.prepare(&w.sql).expect("cold prepare");
                let rows = prepared(&mut reader,&stmt,sink);
                reader.close(); drop(reader); opened.close().expect("cold close"); rows
            } else if w.mode=="parallel" {parallel(db.as_ref().unwrap(),&w.sql,sink)} else {
                let s = session.as_mut().unwrap();
                if w.mode=="rollback" { query(s,"BEGIN",&mut Checksum::new()); }
                let rows = prepared(s,statement.as_ref().unwrap(),sink);
                if w.mode=="rollback" { query(s,"ROLLBACK",&mut Checksum::new()); }
                rows
            };
            let elapsed = start.elapsed().as_nanos();
            if i<w.warmup {warmups.push(elapsed)} else {samples.push(elapsed); count+=rows}
        }
        if w.mode=="rollback" || w.mode=="durable" { count+=query(session.as_mut().unwrap(),"SELECT count(*), sum(v) FROM t",&mut sum); }
        println!("{{\"name\":\"{}\",\"mode\":\"{}\",\"runtime\":\"{}\",\"warmup\":{},\"iterations\":{},\"rows\":{},\"checksum\":\"{:016x}\",\"cost\":{},\"samples_ns\":{:?},\"warmup_ns\":{:?},\"allocated_bytes\":null,\"gc_collections\":null}}",w.name,w.mode,env!("JED_BENCH_RUSTC"),w.warmup,w.iterations,count,sum.value,sum.cost,samples,warmups);
        if let Some(mut s) = session {s.close();drop(s)}
        if let Some(db) = db {db.close().expect("close benchmark")}
    }
}
