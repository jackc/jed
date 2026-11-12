//! Lazy leaf edits (leaf_lazy.rs) against the eager paths: the same write workload must give the
//! same query results and the same file bytes either way. Byte-level and switch-driven, so out of
//! the conformance corpus's reach.
use super::*;
use crate::file::Locking;
use crate::format::leaf_patch_check;
use crate::pmap::{leaf_lazy, leaf_sizes_check};
use crate::{CreateOptions, OpenOptions};

fn open_options() -> OpenOptions {
    OpenOptions {
        skip_fsync: true,
        locking: Locking::Exclusive,
        ..OpenOptions::default()
    }
}

/// Run a write workload against a file-backed database, committing between statements so the
/// leaves it touches are clean packed pages, and return a transcript of every query result and
/// the file's bytes after a final reopen.
fn lazy_workload(lazy: bool) -> (String, Vec<u8>) {
    leaf_lazy::set(lazy);
    let path = std::env::temp_dir().join(format!(
        "jed_leaf_lazy_{}_{}.jed",
        std::process::id(),
        if lazy { "lazy" } else { "eager" }
    ));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(CreateOptions {
        path: Some(path.clone()),
        page_size: 4096,
        skip_fsync: true,
        locking: Locking::Exclusive,
        ..CreateOptions::default()
    })
    .unwrap();
    let mut sess = db.session(SessionOptions::default());
    let mut out = String::new();

    macro_rules! exec {
        ($($arg:tt)*) => {{
            let sql = format!($($arg)*);
            if let Err(e) = sess.query_outcome(&sql, &[]) {
                panic!("{sql}: {e:?}");
            }
        }};
    }
    macro_rules! query {
        ($sql:expr) => {{
            out.push_str($sql);
            out.push('\n');
            let rows: Vec<_> = sess
                .query($sql, &[])
                .unwrap()
                .collect::<Result<_>>()
                .unwrap();
            for row in rows {
                let cells: Vec<String> = row.iter().map(|v| v.render()).collect();
                out.push_str("  ");
                out.push_str(&cells.join("|"));
                out.push('\n');
            }
        }};
    }
    macro_rules! reopen {
        () => {{
            drop(sess);
            db.close().unwrap();
            db = Database::open_with_options(&path, open_options()).unwrap();
            sess = db.session(SessionOptions::default());
        }};
    }

    // Integers, a text with few distinct values, a text with unique values, an integer array, a
    // boolean, NULLs.
    exec!(
        "CREATE TABLE w (id bigint PRIMARY KEY, a bigint, b integer, c text, d text, e bigint[],
        f bigint, g integer, h text, k bigint, m boolean)"
    );
    exec!("CREATE INDEX w_c ON w (c)");
    exec!(
        "INSERT INTO w SELECT x, x * 7, CASE WHEN x % 11 = 0 THEN NULL ELSE x % 100 END,
        'status-' || (x % 5), 'row-' || x, ARRAY[x, x + 1, x % 9], 1000000 + x, x % 3,
        CASE WHEN x % 4 = 0 THEN NULL ELSE 'h' || (x % 2) END, x * 1000, x % 2 = 0
        FROM generate_series(1, 4000) AS g(x)"
    );
    reopen!();

    // Single-row statements, each its own commit: one column of one row, of every kind.
    for i in 0..120 {
        let id = 1 + (i * 331) % 4000;
        match i % 8 {
            0 => exec!("UPDATE w SET a = a + 1 WHERE id = {id}"),
            1 => exec!("UPDATE w SET c = 'changed-' || (id % 3) WHERE id = {id}"),
            2 => exec!("UPDATE w SET d = 'second-' || id WHERE id = {id}"),
            3 => exec!("UPDATE w SET e = ARRAY[id, id * 1000000007] WHERE id = {id}"),
            4 => exec!("UPDATE w SET h = NULL, b = NULL WHERE id = {id}"),
            5 => exec!("UPDATE w SET g = g WHERE id = {id}"), // every column unchanged
            6 => exec!("UPDATE w SET k = 9000000000000 + id WHERE id = {id}"),
            _ => exec!("UPDATE w SET e = NULL, c = NULL WHERE id = {id}"),
        }
    }
    query!("SELECT count(*), sum(a), sum(b), sum(k), count(c), count(h) FROM w");

    // Several rows of one leaf in one statement, then the same leaves again.
    exec!("UPDATE w SET a = a - 3 WHERE id BETWEEN 2000 AND 2100");
    exec!("UPDATE w SET c = 'bulk' WHERE id BETWEEN 2050 AND 2150");
    exec!("UPDATE w SET f = f WHERE id BETWEEN 2000 AND 2400");
    exec!("UPDATE w SET d = d || '!' WHERE id % 50 = 0");
    exec!("UPDATE w SET m = NOT m WHERE id % 97 = 0");
    exec!("UPDATE w SET m = NULL WHERE id = 2222");

    // Deletes and inserts on leaves patched above.
    for i in 0..40 {
        let id = 1 + (i * 331) % 4000;
        exec!("DELETE FROM w WHERE id = {}", id + 1);
        exec!(
            "INSERT INTO w VALUES ({}, 1, NULL, 'status-1', 'new', '{{}}', 5, 6, 'h0', 7, true)",
            100000 + id
        );
    }
    exec!(
        "INSERT INTO w VALUES (2, 14, 2, 'status-2', 'back', ARRAY[2], 5, 6, NULL, 7, false) \
         ON CONFLICT (id) DO NOTHING"
    );
    query!("SELECT id, a, c, d, e, h FROM w WHERE id BETWEEN 2040 AND 2060 ORDER BY id");

    // Rows that grow: enough to split a leaf, then past the inline limit (a compressed or
    // external value, which the patch encoder leaves to the full one).
    for i in 0..30 {
        exec!(
            "UPDATE w SET d = repeat('x', 300) || id WHERE id = {}",
            3000 + i * 7
        );
    }
    exec!("UPDATE w SET d = repeat('long value ', 900) WHERE id = 3500");
    exec!("UPDATE w SET a = 0 WHERE id = 3501");
    exec!("UPDATE w SET d = 'short again' WHERE id = 3500");
    exec!("UPDATE w SET a = 1 WHERE id = 3502");
    for i in 0..12 {
        let id = 3600 + i * 29;
        exec!("UPDATE w SET h = repeat('large-{i} ', 1200) WHERE id = {id}");
        exec!("UPDATE w SET a = a + 1 WHERE id = {}", id + 1);
        exec!("UPDATE w SET k = k + 1 WHERE id = {id}");
        exec!("UPDATE w SET c = 'beside' WHERE id = {}", id - 1);
        if i % 3 == 0 {
            exec!("DELETE FROM w WHERE id = {}", id + 2);
            exec!("UPDATE w SET h = 'small' WHERE id = {id}");
        }
    }

    // A transaction of several statements: later statements find the lazy leaves of earlier
    // ones, and reads inside it see them.
    exec!("BEGIN");
    exec!("UPDATE w SET a = -1 WHERE id = 1200");
    exec!("UPDATE w SET c = 'txn' WHERE id = 1201");
    query!(
        "SELECT id, a, b, c, d, e, f, g, h, k FROM w WHERE id BETWEEN 1195 AND 1205 ORDER BY id"
    );
    exec!("DELETE FROM w WHERE id = 1202");
    exec!("UPDATE w SET k = 1 WHERE id = 1203");
    query!("SELECT count(*), sum(a), sum(k) FROM w WHERE id BETWEEN 1100 AND 1300");
    query!("SELECT id, c FROM w WHERE c = 'txn'");
    exec!("COMMIT");

    // One leaf edited by many statements of a transaction: columns measured one after another as
    // they change, a row replaced twice, a savepoint (a later statement then copies the lazy
    // leaf), a delete and an insert among the replacements, and a partial rollback.
    exec!("BEGIN");
    exec!("UPDATE w SET b = 77 WHERE id = 1400");
    exec!("UPDATE w SET k = 5 WHERE id = 1401");
    exec!("UPDATE w SET e = ARRAY[5, 6] WHERE id = 1402");
    exec!("UPDATE w SET c = 'zz' WHERE id = 1402");
    exec!("UPDATE w SET c = 'status-1', a = 3 WHERE id = 1402");
    exec!("UPDATE w SET k = NULL WHERE id BETWEEN 1380 AND 1420");
    exec!("UPDATE w SET k = 12 WHERE id = 1390");
    exec!("SAVEPOINT s1");
    exec!("UPDATE w SET d = 'sp' WHERE id = 1403");
    query!("SELECT id, a, b, c, d, e, k FROM w WHERE id BETWEEN 1398 AND 1408 ORDER BY id");
    exec!("DELETE FROM w WHERE id = 1404");
    exec!("UPDATE w SET a = 9 WHERE id = 1405");
    exec!("INSERT INTO w VALUES (1404, 1, 2, 'status-9', 'again', ARRAY[4], 5, 6, 'h1', 7, NULL)");
    exec!("UPDATE w SET a = 10, c = 'status-9' WHERE id = 1407");
    exec!("SAVEPOINT s2");
    exec!("UPDATE w SET g = 5 WHERE id = 1408");
    exec!("ROLLBACK TO SAVEPOINT s2");
    exec!("UPDATE w SET h = 'after' WHERE id = 1406");
    query!("SELECT id, a, b, c, d, e, k FROM w WHERE id BETWEEN 1398 AND 1408 ORDER BY id");
    exec!("COMMIT");
    exec!("BEGIN");
    exec!("UPDATE w SET f = 1 WHERE id = 1500");
    exec!("SAVEPOINT s1");
    exec!("UPDATE w SET f = 2, d = 'x' WHERE id = 1501");
    exec!("UPDATE w SET f = 3 WHERE id = 1500");
    exec!("ROLLBACK TO SAVEPOINT s1");
    exec!("UPDATE w SET g = 9 WHERE id = 1502");
    exec!("COMMIT");

    // The same, rolled back.
    exec!("BEGIN");
    exec!("UPDATE w SET a = -2, d = 'gone' WHERE id BETWEEN 700 AND 760");
    exec!("DELETE FROM w WHERE id BETWEEN 720 AND 740");
    exec!("UPDATE w SET e = ARRAY[1, 2, 3] WHERE id = 750");
    query!("SELECT count(*), sum(a) FROM w WHERE id BETWEEN 690 AND 770");
    exec!("ROLLBACK");
    query!("SELECT count(*), sum(a) FROM w WHERE id BETWEEN 690 AND 770");
    reopen!();

    // Range deletes that merge leaves, some of them patched above.
    exec!("DELETE FROM w WHERE id BETWEEN 500 AND 1150");
    exec!("DELETE FROM w WHERE id % 3 = 0 AND id < 3000");
    exec!("UPDATE w SET b = b + 1 WHERE id % 7 = 0");
    exec!("UPDATE w SET a = id");
    for i in 0..20 {
        exec!("UPDATE w SET g = {i} WHERE id = {}", 1300 + i * 13);
    }
    query!("SELECT count(*), sum(a), sum(b), sum(g) FROM w");

    reopen!();
    query!("SELECT * FROM w ORDER BY id");
    drop(sess);
    db.close().unwrap();
    let file = std::fs::read(&path).unwrap();

    // Every leaf of the committed table: the size facts read from its page against a
    // measurement of its decoded rows.
    let engine = Engine::open_with_options(&path, open_options()).unwrap();
    let leaves = engine.committed().store("w").check_packed_leaves();
    assert!(leaves > 0, "no packed leaf was checked");
    drop(engine);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(path.with_extension("jed.lock"));
    (out, file)
}

/// A dirty leaf that keeps its page's rows undecoded, sizes itself from the page and is encoded
/// from the page must produce the rows, query results and file bytes of the eager paths. The
/// workload runs with the size-fact and encoder cross-checks on.
#[test]
fn lazy_leaves_match_eager_leaves() {
    leaf_sizes_check::set(true);
    leaf_patch_check::set(true);
    let (lazy_rows, lazy_file) = lazy_workload(true);
    let (eager_rows, eager_file) = lazy_workload(false);
    leaf_lazy::set(true);
    leaf_sizes_check::set(false);
    leaf_patch_check::set(false);
    assert_eq!(
        lazy_rows, eager_rows,
        "query results differ between lazy and eager leaves"
    );
    assert!(
        lazy_file == eager_file,
        "file bytes differ between lazy and eager leaves ({} and {} bytes)",
        lazy_file.len(),
        eager_file.len()
    );
}

/// Incompressible text: stored in an overflow chain at the root workload's page size.
fn big(seed: u32, len: usize) -> String {
    let mut x = seed;
    let mut s = String::with_capacity(len);
    while s.len() < len {
        x = (x.wrapping_mul(1103515245).wrapping_add(12345)) & 0x7fff_ffff;
        s.push((b'a' + (x % 26) as u8) as char);
    }
    s
}

/// Write tables that stay a single root leaf, some rows holding values stored out of line or
/// compressed. A persisted root leaf is re-faulted for lazy edits only when that cannot change
/// which overflow chains the next commit writes (storage.rs `demote_clean_leaves`).
fn root_workload(lazy: bool) -> (String, Vec<u8>) {
    leaf_lazy::set(lazy);
    let path = std::env::temp_dir().join(format!(
        "jed_leaf_lazy_root_{}_{}.jed",
        std::process::id(),
        if lazy { "lazy" } else { "eager" }
    ));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(CreateOptions {
        path: Some(path.clone()),
        page_size: 512,
        skip_fsync: true,
        locking: Locking::Exclusive,
        ..CreateOptions::default()
    })
    .unwrap();
    let mut sess = db.session(SessionOptions::default());
    let mut out = String::new();

    macro_rules! exec {
        ($($arg:tt)*) => {{
            let sql = format!($($arg)*);
            if let Err(e) = sess.query_outcome(&sql, &[]) {
                panic!("{sql}: {e:?}");
            }
        }};
    }
    macro_rules! query {
        ($sql:expr) => {{
            out.push_str($sql);
            out.push('\n');
            let rows: Vec<_> = sess
                .query($sql, &[])
                .unwrap()
                .collect::<Result<_>>()
                .unwrap();
            for row in rows {
                let cells: Vec<String> = row.iter().map(|v| v.render()).collect();
                out.push_str("  ");
                out.push_str(&cells.join("|"));
                out.push('\n');
            }
        }};
    }
    macro_rules! reopen {
        () => {{
            drop(sess);
            db.close().unwrap();
            db = Database::open_with_options(&path, open_options()).unwrap();
            sess = db.session(SessionOptions::default());
        }};
    }

    exec!("CREATE TABLE r (id integer PRIMARY KEY, n bigint, body text, tag text)");
    exec!("CREATE TABLE s (id integer PRIMARY KEY, n bigint, t text)");
    // A root leaf whose resident rows include an external value, then writes beside it.
    exec!("INSERT INTO r VALUES (1, 10, '{}', 'a')", big(1, 1500));
    exec!("INSERT INTO r VALUES (2, 20, 'small', 'a')");
    exec!("UPDATE r SET n = n + 1 WHERE id = 2");
    exec!("UPDATE r SET tag = 'b' WHERE id = 1");
    exec!("INSERT INTO r VALUES (3, 30, repeat('compressible ', 60), 'c')");
    exec!("UPDATE r SET n = n + 1 WHERE id = 3");
    exec!("UPDATE r SET n = 0");
    query!("SELECT id, n, length(body), tag FROM r ORDER BY id");
    // A root leaf of inline rows only: re-faulted and edited lazily between commits.
    for i in 1..=12 {
        exec!("INSERT INTO s VALUES ({i}, {}, 'v{}')", i * 100, i % 3);
    }
    for i in 1..=12 {
        exec!("UPDATE s SET n = n + {i} WHERE id = {}", (i * 5) % 12 + 1);
    }
    exec!("UPDATE s SET t = 'changed' WHERE id = 4");
    exec!("DELETE FROM s WHERE id = 5");
    exec!("INSERT INTO s VALUES (40, 1, NULL)");
    exec!("BEGIN");
    exec!("UPDATE s SET n = -1 WHERE id = 6");
    exec!("UPDATE s SET t = NULL WHERE id = 7");
    query!("SELECT * FROM s ORDER BY id");
    exec!("COMMIT");
    exec!("UPDATE s SET n = n * 2 WHERE id = 6");
    reopen!();
    // The same after a reopen: the root is a clean page with chains.
    exec!("UPDATE r SET n = 5 WHERE id = 2");
    exec!("UPDATE r SET n = 6 WHERE id = 1");
    exec!("UPDATE r SET body = '{}' WHERE id = 2", big(2, 900));
    exec!("UPDATE r SET tag = 'd' WHERE id = 3");
    exec!("DELETE FROM r WHERE id = 1");
    exec!("INSERT INTO r VALUES (4, 40, '{}', 'e')", big(3, 1200));
    exec!("UPDATE r SET n = n + 1");
    exec!("UPDATE s SET n = n + 1");
    query!("SELECT id, n, length(body), tag FROM r ORDER BY id");
    reopen!();
    query!("SELECT id, n, body, tag FROM r ORDER BY id");
    query!("SELECT * FROM s ORDER BY id");
    drop(sess);
    db.close().unwrap();
    let file = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(path.with_extension("jed.lock"));
    (out, file)
}

/// A single-leaf table's root is re-faulted after a commit only when the next write then stores
/// the same overflow chains as the eager path: the transcript and file bytes must not depend on
/// the switch. Runs with the size-fact and encoder cross-checks on.
#[test]
fn lazy_root_leaves_match_eager_root_leaves() {
    leaf_sizes_check::set(true);
    leaf_patch_check::set(true);
    let (lazy_rows, lazy_file) = root_workload(true);
    let (eager_rows, eager_file) = root_workload(false);
    leaf_lazy::set(true);
    leaf_sizes_check::set(false);
    leaf_patch_check::set(false);
    assert_eq!(
        lazy_rows, eager_rows,
        "query results differ between lazy and eager leaves"
    );
    assert!(
        lazy_file == eager_file,
        "file bytes differ between lazy and eager leaves ({} and {} bytes)",
        lazy_file.len(),
        eager_file.len()
    );
}

/// A table that shrinks back to a single root leaf whose rows were faulted in as references to
/// overflow chains (conformance `dml/spilled_root_leaf.test`), in an in-memory database: the
/// committed storage after every statement and the query results.
fn shrink_workload(lazy: bool) -> String {
    leaf_lazy::set(lazy);
    let db = Database::create(CreateOptions::default()).unwrap();
    let mut sess = db.session(SessionOptions::default());
    let mut out = String::new();

    macro_rules! exec {
        ($($arg:tt)*) => {{
            let sql = format!($($arg)*);
            if let Err(e) = sess.query_outcome(&sql, &[]) {
                panic!("{sql}: {e:?}");
            }
            out.push_str(&format!("storage {}\n", db.storage_bytes("main").unwrap()));
        }};
    }
    macro_rules! query {
        ($sql:expr) => {{
            out.push_str($sql);
            out.push('\n');
            let rows: Vec<_> = sess
                .query($sql, &[])
                .unwrap()
                .collect::<Result<_>>()
                .unwrap();
            for row in rows {
                let cells: Vec<String> = row.iter().map(|v| v.render()).collect();
                out.push_str("  ");
                out.push_str(&cells.join("|"));
                out.push('\n');
            }
        }};
    }

    exec!("CREATE TABLE shrink (id integer PRIMARY KEY, v text)");
    exec!(
        "INSERT INTO shrink VALUES (1, '{}'), (2, '{}')",
        big(12345, 4500),
        big(777, 4500)
    );
    exec!("INSERT INTO shrink SELECT g, repeat('x', 3000) FROM generate_series(3, 40) g");
    query!("SELECT id, length(v), md5(v) FROM shrink WHERE id <= 2 ORDER BY id");
    // The tree collapses to one root leaf holding rows 1 and 2: this commit writes their values
    // into fresh chains.
    exec!("DELETE FROM shrink WHERE id >= 3");
    // Commits that never touch `shrink` free its old chains, then reuse those pages.
    exec!("CREATE TABLE shrink_other (id integer PRIMARY KEY, v text)");
    exec!("INSERT INTO shrink_other SELECT g, repeat('y', 3000) FROM generate_series(1, 40) g");
    exec!("INSERT INTO shrink_other SELECT g, repeat('z', 3000) FROM generate_series(41, 80) g");
    query!("SELECT id, length(v), md5(v) FROM shrink ORDER BY id");
    // Writes to the root leaf beside its spilled values, and to the values themselves.
    exec!("INSERT INTO shrink VALUES (5, 'small')");
    exec!("UPDATE shrink SET v = v || '!' WHERE id = 5");
    exec!("UPDATE shrink SET v = '{}' WHERE id = 2", big(99, 4500));
    exec!("DELETE FROM shrink WHERE id = 1");
    exec!("INSERT INTO shrink_other SELECT g, repeat('w', 3000) FROM generate_series(81, 120) g");
    query!("SELECT id, length(v), md5(v) FROM shrink ORDER BY id");
    query!("SELECT count(*) FROM shrink_other");
    out
}

/// The shrink-to-root workload's committed storage and results do not depend on the switch.
#[test]
fn lazy_shrunk_root_matches_eager_in_memory() {
    leaf_sizes_check::set(true);
    leaf_patch_check::set(true);
    let lazy = shrink_workload(true);
    let eager = shrink_workload(false);
    leaf_lazy::set(true);
    leaf_sizes_check::set(false);
    leaf_patch_check::set(false);
    assert_eq!(
        lazy, eager,
        "storage or results differ between lazy and eager leaves"
    );
}
