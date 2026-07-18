//! Golden-file cross-core test (CLAUDE.md §8). The load-bearing honesty test for the
//! on-disk format: each core must (a) READ a checked-in golden into the expected
//! catalog + rows, and (b) WRITE the same logical database to bytes equal to the
//! golden EXACTLY. Because the format is deterministic, this gives
//! `rust-bytes == golden == go-bytes`, so each core can read the other's output
//! without any live cross-process exchange. Goldens are authored at page_size 256 by
//! spec/fileformat/verify.rb (the independent reference).

use jed::types::ScalarType;
use jed::value::Value;
use jed::{CreateOptions, Database, Session, SessionOptions};

use std::path::PathBuf;

/// The page size the goldens are authored at (small, so the hex stays reviewable).
const GOLDEN_PAGE_SIZE: u32 = 256;

/// A function that builds one of the sample databases the goldens correspond to.
type Builder = fn() -> Session;

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec/fileformat/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn refresh_test_page_crc(image: &mut [u8], page_start: usize) {
    let mut crc = 0xffff_ffffu32;
    for byte in image[page_start..page_start + 12]
        .iter()
        .chain(image[page_start + 16..page_start + GOLDEN_PAGE_SIZE as usize].iter())
    {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    image[page_start + 12..page_start + 16].copy_from_slice(&(!crc).to_be_bytes());
}

/// Load jed's pinned production `JUCD` bundle into the engine-global set so the `unicode`-collated
/// goldens build (set_default_collation / COLLATE) and read back (the file's reference entry resolves
/// its table from a loaded bundle — collation.md §4/§9, slice 3c). Idempotent (global, first-wins).
fn load_unicode() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec/collation/fixtures/unicode.jucd");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    jed::load_unicode_data(&bytes).expect("load unicode.jucd");
}

fn run(db: &mut Session, sql: &str) {
    db.query_outcome(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message));
}

/// `CREATE TABLE t (id i32 PRIMARY KEY, v i16)` with 20 rows (id 3 has a NULL
/// value) — enough rows to span more than one data page at page_size 256.
fn pk_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i16)");
    for i in 1..=20i64 {
        let v = if i == 3 {
            "NULL".to_string()
        } else {
            (i * 10).to_string()
        };
        run(&mut db, &format!("INSERT INTO t VALUES ({i}, {v})"));
    }
    db
}

fn one_table_empty_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i16)");
    db
}

fn row_count_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY)");
    run(&mut db, "INSERT INTO t VALUES (1), (2), (3)");
    db
}

fn statistics_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE fresh (id i32 PRIMARY KEY, v text)");
    run(&mut db, "CREATE TABLE stale (id i32 PRIMARY KEY, v text)");
    run(
        &mut db,
        "INSERT INTO fresh VALUES (1, 'a'), (2, 'a'), (3, 'b'), (4, NULL)",
    );
    run(
        &mut db,
        "INSERT INTO stale VALUES (1, 'x'), (2, 'x'), (3, NULL)",
    );
    run(&mut db, "ANALYZE fresh");
    run(&mut db, "ANALYZE stale");
    run(&mut db, "INSERT INTO stale VALUES (4, 'y')");
    db
}

/// A table with a COMPOSITE primary key (constraints.md §3) — the stored key is the
/// concatenation of the members' encodings (4-byte i32 ‖ 2-byte i16, encoding.md §2.3).
/// Rows insert in ascending tuple order (the tree shape is order-sensitive), with a negative
/// first component and first-component ties broken by the second.
fn composite_pk_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (a i32, b i16, v i16, PRIMARY KEY (a, b))",
    );
    for (a, b, v) in [
        (-2, 5, 10),
        (1, 1, 20),
        (1, 2, 30),
        (1, 3, 40),
        (2, 0, 50),
        (2, 1, 60),
        (3, 7, 70),
        (3, 9, 80),
    ] {
        run(&mut db, &format!("INSERT INTO t VALUES ({a}, {b}, {v})"));
    }
    db
}

/// A table with CHECK constraints (constraints.md §4) — exercises the v4 catalog check
/// list: an auto-named single-column check, an explicitly-named multi-column check, and a
/// check whose persisted text exercises the token rendering (string literal with a doubled
/// quote, decimal literals, `>=`/`<=`), stored in name order
/// (price_range < t_b_check < t_note_check).
fn check_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (a int PRIMARY KEY, b int CHECK (b > 0), price numeric(8,2), \
         CONSTRAINT price_range CHECK (price >= 0.50 AND price <= 9999.99), note text, \
         CHECK (note = 'ok' OR note = 'a''b'))",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (1, 5, 1.00, 'ok'), (2, NULL, 9999.99, 'a''b'), \
         (3, 100, 0.50, 'ok')",
    );
    db
}

/// A table with SECONDARY INDEXES (v5 — spec/design/indexes.md): the catalog reshape +
/// the index trees. The PK list order (b, a) differs from declaration order (the lifted
/// composite-PK narrowing); `i_u` covers a nullable uuid column holding a NULL (the
/// encoding.md §2.2 presence tag in stored index order — NULL last), and the unnamed
/// index auto-names to `t_a_b_idx`. Index records have empty payloads (key only).
fn index_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (a i32, b i32, u uuid, PRIMARY KEY (b, a))",
    );
    run(&mut db, "CREATE INDEX i_u ON t (u)");
    run(&mut db, "CREATE INDEX ON t (a, b)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, 10, '550e8400-e29b-41d4-a716-446655440000'),          (2, 10, NULL), (3, 20, '00000000-0000-0000-0000-000000000000')",
    );
    db
}

/// A table with UNIQUE indexes (v6 — the per-index flags byte, indexes.md §8): `t_v_key`
/// (a UNIQUE constraint's auto-name) over a nullable column holding two NULLs (NULLS
/// DISTINCT — both stored), the named two-column constraint `wv`, a CREATE UNIQUE INDEX
/// `uq`, and the plain index `nu` (flags 0 beside flags 1).
fn unique_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32, w i32, UNIQUE (v), CONSTRAINT wv UNIQUE (w, v))",
    );
    run(&mut db, "CREATE INDEX nu ON t (v)");
    run(&mut db, "CREATE UNIQUE INDEX uq ON t (w)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, 10, 100), (2, NULL, 200), (3, NULL, 300)",
    );
    db
}

/// A table with an EXPRESSION index key (v26 — the per-index `0xFFFF` sentinel + canonical text,
/// indexes.md §6): a plain column index `t_email_idx` beside the UNIQUE expression index
/// `t_lower_idx` over `lower(email)`. The table is EMPTY (both index trees empty, root 0), so the
/// fixture isolates the v26 catalog change. Must match verify.rb's EXPR_INDEX_TABLE.
fn expr_index_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, email text)");
    run(&mut db, "CREATE INDEX ON t (email)");
    run(&mut db, "CREATE UNIQUE INDEX ON t (lower(email))");
    db
}

/// A table with PARTIAL index predicates (v27 — the `index_flags` bit1 + the canonical predicate
/// text after `index_root_page`, indexes.md §9): a plain partial index `t_amt_idx` and a UNIQUE
/// partial index `t_uact` (both `WHERE status = 'active'`) beside a non-partial `t_status_idx`
/// (bit1 clear, byte-identical to v26). The table is EMPTY (all three trees empty, root 0), so the
/// fixture isolates the v27 catalog change. Must match verify.rb's PARTIAL_INDEX_TABLE.
fn partial_index_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, status text, amt i32)",
    );
    run(&mut db, "CREATE INDEX ON t (amt) WHERE status = 'active'");
    run(
        &mut db,
        "CREATE UNIQUE INDEX t_uact ON t (amt) WHERE status = 'active'",
    );
    run(&mut db, "CREATE INDEX ON t (status)");
    db
}

/// Tables with FOREIGN KEY constraints (v11 — spec/design/constraints.md §6): pins the catalog
/// foreign-key list. Parent `p` (a PK + two UNIQUE constraints, the FK targets); child `c` with
/// four FKs covering every shape — a named FK to the UNIQUE column (`c_code_fk`), a self-reference
/// to the PK (`c_mgr_fkey`), an auto-named FK to the PK (`c_pid_fkey`), and an auto-named COMPOSITE
/// FK to the two-column UNIQUE (`c_x_y_fkey`). Their action bytes collectively pin every v30
/// action code. Must match the Ruby reference's FK_TABLE (spec/fileformat/verify.rb).
fn fk_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE p (pid i32 PRIMARY KEY, code i32 UNIQUE, a i32, b i32, UNIQUE (a, b))",
    );
    run(
        &mut db,
        "INSERT INTO p VALUES (1, 100, 10, 20), (2, 200, 30, 40)",
    );
    run(
        &mut db,
        "CREATE TABLE c (id i32 PRIMARY KEY, pid i32, pcode i32, x i32, y i32, mgr i32, \
         FOREIGN KEY (pid) REFERENCES p (pid) ON DELETE RESTRICT, \
         CONSTRAINT c_code_fk FOREIGN KEY (pcode) REFERENCES p (code) ON DELETE SET NULL, \
         FOREIGN KEY (x, y) REFERENCES p (a, b) ON DELETE CASCADE ON UPDATE SET DEFAULT, \
         FOREIGN KEY (mgr) REFERENCES c (id))",
    );
    run(
        &mut db,
        "INSERT INTO c VALUES (10, 1, 100, 10, 20, NULL), (11, 2, 200, 30, 40, 10)",
    );
    db
}

/// A table with ARRAY (`T[]`) columns (v10 — spec/design/array.md): pins the catalog array-column
/// entry (type_code 15 + the element-type descriptor, §3) and the compact value body (§4). An
/// `i32[]` (fixed-width elements: no per-element length prefix) and a `text[]`; row 2 has an EMPTY
/// array (ndim=0), row 3 a NULL element (the HAS_NULLS bitmap) and a whole-value NULL array (the
/// lone 0x01 tag). Must match the Ruby reference's ARRAY_TABLE (spec/fileformat/verify.rb).
fn array_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, xs i32[], tags text[])",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (1, ARRAY[10, 20, 30], ARRAY['a', 'b'])",
    );
    run(&mut db, "INSERT INTO t VALUES (2, '{40,50}', '{}')");
    run(&mut db, "INSERT INTO t VALUES (3, ARRAY[1, NULL, 3], NULL)");
    // Row 4 pins the §12 shapes: a 2-D i32[] and a custom-lower-bound text[] (the lb i32 field).
    run(
        &mut db,
        "INSERT INTO t VALUES (4, ARRAY[ARRAY[10,20],ARRAY[30,40]], '[2:3]={x,y}')",
    );
    db
}

/// A table with a GIN inverted index (v13 — the per-index index_kind byte, spec/design/gin.md):
/// `i_nums_gin` over an i32[] column (kind 1) beside an ordinary ordered index `i_n` over a
/// scalar column (kind 0 — a btree index cannot sit on the array column). Rows exercise term DEDUP
/// (row 2's duplicate 20), an EMPTY and a NULL whole-value array (rows 3/4 → no entries), and a
/// NULL element (row 5). Rows are inserted before the indexes so each builds via the sorted-bulk
/// path, matching the Ruby reference's GIN_ARRAY_TABLE.
/// `CREATE TABLE t (id i32 PRIMARY KEY, r i32range, br i64range)` with rows exercising every
/// range flags bit + the canonical-`[)` storage (spec/design/ranges.md §4): a finite `[)`, an
/// inclusive-upper literal that canonicalizes (`[1,5]` → `[1,6)`), the EMPTY range, infinite bounds
/// (lower-only, both), a NULL range, an exclusive-lower literal with infinite upper (`(5,)` →
/// `[6,)`), and a singleton (`[1,1]` → `[1,2)`). Pins range_table.jed cross-core.
fn range_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, r i32range, br i64range)",
    );
    run(&mut db, "INSERT INTO t VALUES (1, '[1,5)', '[10,20)')");
    run(&mut db, "INSERT INTO t VALUES (2, '[1,5]', NULL)");
    run(&mut db, "INSERT INTO t VALUES (3, 'empty', '(,100)')");
    run(&mut db, "INSERT INTO t VALUES (4, '(,)', '(5,)')");
    run(&mut db, "INSERT INTO t VALUES (5, NULL, '[1,1]')");
    db
}

/// A table with an i32range PRIMARY KEY — the first CONTAINER key (encoding.md §2.11). The
/// range-bounds key (empty/±∞/inclusivity framing around the i32 element key) lands in the key slot.
/// Rows are inserted in ASCENDING range_total_cmp order (empty, unbounded-lower, fully-unbounded,
/// then finite-bound) to match verify.rb's ascending-key tree builder.
fn range_pk_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (k i32range PRIMARY KEY, v i32)");
    run(&mut db, "INSERT INTO t VALUES ('empty', 0)");
    run(&mut db, "INSERT INTO t VALUES ('(,5)', 1)");
    run(&mut db, "INSERT INTO t VALUES ('(,)', 2)");
    run(&mut db, "INSERT INTO t VALUES ('[1,5)', 3)");
    run(&mut db, "INSERT INTO t VALUES ('[2,4)', 4)");
    run(&mut db, "INSERT INTO t VALUES ('[2,)', 5)");
    db
}

/// A table with an i32[] PRIMARY KEY — the SECOND container key (encoding.md §2.14), the first whose
/// key length varies with the element count. The array-elements-terminated key (per-element markers +
/// terminator + shape suffix) lands in the key slot. Rows are inserted in ASCENDING array_total_cmp
/// order (empty, shorter-prefix, element-wise, NULL element last) to match verify.rb's ascending-key
/// tree builder. Pins array_pk_table.jed cross-core.
fn array_pk_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE k (key i32[] PRIMARY KEY, v i32)");
    run(&mut db, "INSERT INTO k VALUES ('{}', 40)");
    run(&mut db, "INSERT INTO k VALUES ('{1,2}', 20)");
    run(&mut db, "INSERT INTO k VALUES ('{1,2,3}', 10)");
    run(&mut db, "INSERT INTO k VALUES ('{1,NULL}', 50)");
    run(&mut db, "INSERT INTO k VALUES ('{2}', 60)");
    db
}

/// A table with a `json` column (verbatim text body, type_code 18 — spec/design/json.md §4). The
/// stored bytes are the input text exactly (whitespace/key-order preserved), so this pins the
/// length-prefixed text-shaped json body. Pins json_table.jed cross-core.
fn json_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, j json)");
    run(&mut db, "INSERT INTO t VALUES (1, '{\"a\": 1}')");
    run(&mut db, "INSERT INTO t VALUES (2, '[1, 2, 3]')");
    run(&mut db, "INSERT INTO t VALUES (3, NULL)");
    db
}

/// A table with a `jsonb` column (the canonical tagged-node tree, type_code 19 —
/// spec/design/json.md §2). The rows exercise every node tag: an object (NTAG_OBJECT) with a
/// number (NTAG_NUMBER), a nested array (NTAG_ARRAY) of a boolean (NTAG_TRUE) and JSON null
/// (NTAG_NULL); a bare string (NTAG_STRING); a bare number; and a SQL NULL. Pins jsonb_table.jed.
fn jsonb_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, j jsonb)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, '{\"a\": 1, \"b\": [true, null]}')",
    );
    run(&mut db, "INSERT INTO t VALUES (2, '\"hello\"')");
    run(&mut db, "INSERT INTO t VALUES (3, '42')");
    run(&mut db, "INSERT INTO t VALUES (4, NULL)");
    db
}

fn gin_array_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, nums i32[], n i32)",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (1, '{10,20,30}', 1), (2, '{20,20,40}', 2), (3, '{}', 3), (4, NULL, 4), (5, '{10,NULL,50}', 5)",
    );
    run(&mut db, "CREATE INDEX i_n ON t (n)");
    run(&mut db, "CREATE INDEX i_nums_gin ON t USING gin (nums)");
    db
}

/// `i_tags_gin` over a `uuid[]` column (kind 1) — the non-integer GIN element-type golden
/// (spec/design/gin.md §3/§4): each GIN term is the element's 16-byte `uuid-raw16` key encoding,
/// so entries are `encode_uuid(term) ‖ storage_key` (empty payload), pinning that a uuid-element
/// GIN index serializes byte-identically across cores. Rows mirror `gin_array_table_db`: term DEDUP
/// (row 2's duplicate bb), an EMPTY and a NULL whole-value array (rows 3/4 → no entries), and a NULL
/// element (row 5). An ordinary ordered index `i_n` sits beside it (kind 0).
fn gin_uuid_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, tags uuid[], n i32)",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES \
         (1, '{00000000-0000-0000-0000-0000000000aa,00000000-0000-0000-0000-0000000000bb,00000000-0000-0000-0000-0000000000cc}', 1), \
         (2, '{00000000-0000-0000-0000-0000000000bb,00000000-0000-0000-0000-0000000000bb,00000000-0000-0000-0000-0000000000dd}', 2), \
         (3, '{}', 3), \
         (4, NULL, 4), \
         (5, '{00000000-0000-0000-0000-0000000000aa,NULL,00000000-0000-0000-0000-0000000000ee}', 5)",
    );
    run(&mut db, "CREATE INDEX i_n ON t (n)");
    run(&mut db, "CREATE INDEX i_tags_gin ON t USING gin (tags)");
    db
}

/// A GiST index over an `i32range` column (kind 2, v20 — spec/design/gist.md GX1): pins the
/// `index_kind = 2` byte and the persisted R-tree (page types 5/6). 6 bounded `[)` ranges + one
/// empty range force a median split at `GIST_FANOUT = 4`, so the on-disk tree is two levels (an
/// interior root over two leaves), exercising both page types + post-order page allocation. Row 8's
/// NULL range is not indexed. Mirrors the Ruby reference's `GIST_RANGE_TABLE`.
fn gist_range_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, '[1,5)'), (2, '[10,20)'), (3, '[3,8)'), (4, '[100,200)'), \
         (5, '[50,60)'), (6, '[15,25)'), (7, 'empty'), (8, NULL)",
    );
    run(&mut db, "CREATE INDEX t_r_gist ON t USING gist (r)");
    db
}

/// A GiST index over a SCALAR `i32` column — the scalar `=` opclass (kind 2, v20 — spec/design/
/// gist.md GX2): pins the scalar bounding-key bytes (`[min,max]` over the order-preserving key
/// encoding, distinguished from a range bound by the indexed column's catalog type). 8 rows with
/// duplicate room numbers force a median split at `GIST_FANOUT = 4`, so the on-disk tree is two
/// levels, exercising both page types + post-order allocation with the scalar bound codec. Row 9's
/// NULL is not indexed. Mirrors the Ruby reference's `GIST_SCALAR_TABLE`.
fn gist_scalar_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, room i32)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 10), (4, 30), (5, 20), (6, 40), (7, 10), \
         (8, 50), (9, NULL)",
    );
    run(&mut db, "CREATE INDEX t_room_gist ON t USING gist (room)");
    db
}

/// An `EXCLUDE USING gist (room WITH =, during WITH &&)` constraint (GX3, v21 — spec/design/gist.md
/// §7/§8): pins the per-table exclusion catalog list (name + backing index name + the
/// `(column, operator)` element vector) AND the **multi-column** GiST node bytes (each leaf bound a
/// scalar `[min,max]` component concatenated with a range component). 7 rows force a median split at
/// `GIST_FANOUT = 4`, so the backing R-tree is two levels. Row 8's NULL room is exempt (not indexed).
/// Mirrors the Ruby reference's `GIST_EXCLUDE_TABLE`.
fn gist_exclude_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE booking (id i32 PRIMARY KEY, room i32, during i32range, \
         EXCLUDE USING gist (room WITH =, during WITH &&))",
    );
    run(
        &mut db,
        "INSERT INTO booking VALUES (1, 101, '[10,20)'), (2, 101, '[20,30)'), \
         (3, 102, '[10,20)'), (4, 102, '[30,40)'), (5, 103, '[10,20)'), \
         (6, 104, '[50,60)'), (7, 105, '[1,5)'), (8, NULL, '[10,20)')",
    );
    db
}

/// A table with no primary key — exercises the stored synthetic i64 rowid key.
fn nopk_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE r (a i16, b i64)");
    for (a, b) in [(7, 70), (8, 80), (9, 90)] {
        run(&mut db, &format!("INSERT INTO r VALUES ({a}, {b})"));
    }
    db
}

/// 18 rows whose wide text padding forces a HEIGHT-2 tree (an interior node whose children are
/// themselves interior nodes) at page_size 256 — exercises interior-of-interior child pointers and
/// post-order page allocation across a deeper tree (spec/fileformat/format.md).
/// Degenerate interior fan-out (format.md "Interior node" / "Fan-out"; bplus-reshape.md §4.2):
/// a secondary index over near-`RECORD_MAX(0)` text keys — each 112-byte entry key packs two per
/// index leaf, and two separators overflow an interior page, so the second leaf split takes the
/// pinned `N = 2 → m = 1` interior split and leaves a legal N = 0 interior on disk. The table
/// rows externalize their (incompressible filler64) text. Must match verify.rb's MAX_SEP_TABLE.
fn max_sep_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE m (id i32 PRIMARY KEY, s text)");
    run(&mut db, "CREATE INDEX i_s ON m (s)");
    let tail = filler_text(104);
    for i in 0..6u8 {
        let s = format!("{}{}", (b'A' + i) as char, tail);
        run(&mut db, &format!("INSERT INTO m VALUES ({}, '{s}')", i + 1));
    }
    db
}

fn tall_tree_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)");
    for i in 1..=18i64 {
        let pad = format!("row-{i:02}-{}", "x".repeat(48));
        run(&mut db, &format!("INSERT INTO t VALUES ({i}, '{pad}')"));
    }
    db
}

/// A table with a text column — exercises the value codec's text branch (u16 length +
/// UTF-8 bytes): the empty string, an embedded quote, a 2-byte char (é), a NULL text
/// value, and a 4-byte astral char (😀). The PK stays i32 (no text key this slice).
fn text_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, s text)");
    run(&mut db, "INSERT INTO t VALUES (1, 'alice')");
    run(&mut db, "INSERT INTO t VALUES (2, '')");
    run(&mut db, "INSERT INTO t VALUES (3, 'O''Brien')");
    run(&mut db, "INSERT INTO t VALUES (4, 'café')");
    run(&mut db, "INSERT INTO t VALUES (5, NULL)");
    run(&mut db, "INSERT INTO t VALUES (6, '😀')");
    db
}

/// A table with a bounded `varchar(5)` column beside an unbounded `text` column — the v22
/// text-column `u32 varchar_max_len` typmod slot (spec/design/types.md §15). Stored values are
/// within the limit. Must match `spec/fileformat/verify.rb`'s `VARCHAR_TABLE`.
fn varchar_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, code varchar(5), note text)",
    );
    run(&mut db, "INSERT INTO t VALUES (1, 'alice', 'hi')");
    run(&mut db, "INSERT INTO t VALUES (2, 'ab', NULL)");
    run(&mut db, "INSERT INTO t VALUES (3, '', 'long note text')");
    db
}

/// A table with a boolean column — exercises the value codec's boolean branch (a single
/// bool-byte, 0x00 false / 0x01 true) plus a NULL boolean. The PK stays i32 (the boolean
/// PRIMARY KEY case is `bool_pk_table_db`).
fn bool_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, flag boolean)");
    run(&mut db, "INSERT INTO t VALUES (1, TRUE)");
    run(&mut db, "INSERT INTO t VALUES (2, FALSE)");
    run(&mut db, "INSERT INTO t VALUES (3, NULL)");
    db
}

/// A table with a boolean PRIMARY KEY (the second golden with a NON-integer stored key, after
/// uuid) — the `bool-byte` key encoding (bare 1 byte 0x00 false / 0x01 true, no presence tag
/// since a PK is NOT NULL, spec/design/encoding.md §2.9), plus a nullable boolean value column.
/// Rows go in via INSERT and the store sorts them into key (byte) order: false (0x00) then true
/// (0x01). Must match spec/fileformat/verify.rb's BOOL_PK_TABLE.
fn bool_pk_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (k boolean PRIMARY KEY, v boolean)");
    run(&mut db, "INSERT INTO t VALUES (FALSE, TRUE)");
    run(&mut db, "INSERT INTO t VALUES (TRUE, NULL)");
    db
}

/// A table with a text PRIMARY KEY (the first golden with a VARIABLE-WIDTH non-integer stored
/// key) — the `text-terminated-escape` key encoding (encoding.md §2.4). Rows go in via INSERT and
/// the store sorts them into key (code-point / byte) order: "" < "Zeta"(0x5A) < "apple"(0x61) <
/// "banana"(0x62) < "é"(0xC3). Must match spec/fileformat/verify.rb's TEXT_PK_TABLE.
fn text_pk_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (k text PRIMARY KEY, v i32)");
    run(&mut db, "INSERT INTO t VALUES ('', 4)");
    run(&mut db, "INSERT INTO t VALUES ('Zeta', NULL)");
    run(&mut db, "INSERT INTO t VALUES ('apple', 2)");
    run(&mut db, "INSERT INTO t VALUES ('banana', 3)");
    run(&mut db, "INSERT INTO t VALUES ('é', 5)");
    db
}

/// A table with a bytea PRIMARY KEY (the `bytea-terminated-escape` key encoding, encoding.md §2.6)
/// — like text but over raw bytes, so the embedded-0x00 escape is exercised. The store sorts into
/// unsigned-byte (key) order: '' < \x00 < \x61 < \x6100ff62 < \x6161 < \x62. Must match
/// spec/fileformat/verify.rb's BYTEA_PK_TABLE.
fn bytea_pk_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (k bytea PRIMARY KEY, v i32)");
    run(&mut db, r"INSERT INTO t VALUES ('\x', 5)");
    run(&mut db, r"INSERT INTO t VALUES ('\x00', 6)");
    run(&mut db, r"INSERT INTO t VALUES ('\x61', 1)");
    run(&mut db, r"INSERT INTO t VALUES ('\x6100ff62', 4)");
    run(&mut db, r"INSERT INTO t VALUES ('\x6161', 2)");
    run(&mut db, r"INSERT INTO t VALUES ('\x62', 3)");
    db
}

/// A table with an (unconstrained) decimal PRIMARY KEY (the `decimal-order-preserving` key
/// encoding, encoding.md §2.5) — the first variable-width SIGNED key. The store sorts into numeric
/// (= key) order: -2.5 < -0.5 < 0 < 0.25 < 1.5 < 10 < 100.50; "100.50" stores scale 2 in its value
/// body but normalizes in the key. Must match spec/fileformat/verify.rb's DECIMAL_PK_TABLE.
fn decimal_pk_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (k decimal PRIMARY KEY, v i32)");
    run(&mut db, "INSERT INTO t VALUES (-2.5, 6)");
    run(&mut db, "INSERT INTO t VALUES (-0.5, 5)");
    run(&mut db, "INSERT INTO t VALUES (0, 4)");
    run(&mut db, "INSERT INTO t VALUES (0.25, 1)");
    run(&mut db, "INSERT INTO t VALUES (1.5, 2)");
    run(&mut db, "INSERT INTO t VALUES (10, 3)");
    run(&mut db, "INSERT INTO t VALUES (100.50, 7)");
    db
}

/// A table with an (unconstrained) interval PRIMARY KEY (the interval-span-i128 encoding,
/// encoding.md §2.10) — the first fixed-width SIGNED 16-byte key. Stored in canonical-span
/// (= key) order: -1 mon < -1 day < 0 < 1 sec < 1 day < 1 mon < 100 years. All spans distinct
/// (span-equal intervals would collide on the span key). Inserted in ascending key order — the
/// builder splits a node by inserting ascending (verify.rb `build_tree`), so the core must match
/// that order for byte-identical pages; the out-of-order-insert proof lives in the conformance test.
fn interval_pk_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (k interval PRIMARY KEY, v i32)");
    run(&mut db, "INSERT INTO t VALUES ('-1 mon', 6)");
    run(&mut db, "INSERT INTO t VALUES ('-1 day', 5)");
    run(&mut db, "INSERT INTO t VALUES ('0 seconds', 4)");
    run(&mut db, "INSERT INTO t VALUES ('1 sec', 1)");
    run(&mut db, "INSERT INTO t VALUES ('1 day', 2)");
    run(&mut db, "INSERT INTO t VALUES ('1 mon', 3)");
    run(&mut db, "INSERT INTO t VALUES ('100 years', 7)");
    db
}

/// A table with a decimal column — exercises the value codec's decimal branch (flags + u16
/// scale + u16 ndigits + base-10⁴ groups) and the catalog typmod: an unconstrained `numeric`
/// column `d` and a constrained `numeric(10,2)` column `m` (values already at scale 2, so a
/// no-op coercion). Covers positive, negative, zero, a multi-group coefficient, and a NULL.
fn decimal_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, d numeric, m numeric(10,2))",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (1, 1.50, 1.50), (2, -12345.6789, -12.34), \
         (3, 0.00, 0.00), (4, 100000000.000001, 100.00), (5, NULL, NULL)",
    );
    db
}

/// A table with a bytea column — exercises the value codec's bytea branch (u16 length + raw
/// bytes): a multi-byte value (a-f hex), the empty byte string, embedded 0x00 bytes, a high
/// byte (0xFF), a NULL, and a lone 0x00. The PK stays i32 (no bytea key this slice).
/// Literals are the `\x` hex input form, adapting to the bytea column (types.md §6).
fn bytea_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, b bytea)");
    run(&mut db, "INSERT INTO t VALUES (1, '\\xdeadbeef')");
    run(&mut db, "INSERT INTO t VALUES (2, '\\x')");
    run(&mut db, "INSERT INTO t VALUES (3, '\\x000102')");
    run(&mut db, "INSERT INTO t VALUES (4, '\\xff')");
    run(&mut db, "INSERT INTO t VALUES (5, NULL)");
    run(&mut db, "INSERT INTO t VALUES (6, '\\x00')");
    db
}

/// Incompressible filler (spec/fileformat/format.md "Fixtures"): xorshift32(seed "JEDB") mapped
/// to a 64-char alphabet (text) or raw bytes (bytea). High-entropy, so the LZ4 encoder never wins
/// store-smaller and the value deterministically stays PLAIN. Mirrors verify.rb's filler_text /
/// filler_bytes; each call restarts at the seed.
const ALPHA64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const FILLER_SEED: u32 = 0x4A45_4442;

fn filler_step(mut x: u32) -> u32 {
    x ^= x << 13;
    x ^= x >> 17;
    x ^ (x << 5)
}

fn filler_text(n: usize) -> String {
    let mut x = FILLER_SEED;
    let mut out = String::with_capacity(n);
    for _ in 0..n {
        x = filler_step(x);
        out.push(ALPHA64[(x % 64) as usize] as char);
    }
    out
}

fn filler_bytes_hex(n: usize) -> String {
    let mut x = FILLER_SEED;
    let mut out = String::with_capacity(n * 2);
    for _ in 0..n {
        x = filler_step(x);
        out.push_str(&format!("{:02x}", x % 256));
    }
    out
}

/// A table with large INCOMPRESSIBLE text + bytea values that spill OUT-OF-LINE PLAIN to overflow
/// pages (spec/design/large-values.md §12): at page_size 256 a ~600/300-byte value exceeds
/// RECORD_MAX (114); compression is attempted first (Slice B) but rejected by store-smaller, so
/// the record holds a 0x02 pointer and the raw bytes live in a page_type-4 chain. Row 1 spills
/// both columns (multi-page chains), row 2 stays inline, row 3 is NULL/NULL. Must match the Ruby
/// reference's OVERFLOW_TABLE (spec/fileformat/verify.rb).
fn overflow_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, body text, blob bytea)",
    );
    run(
        &mut db,
        &format!(
            "INSERT INTO t VALUES (1, '{}', '\\x{}')",
            filler_text(600),
            filler_bytes_hex(300)
        ),
    );
    run(&mut db, "INSERT INTO t VALUES (2, 'small', '\\xcafe')");
    run(&mut db, "INSERT INTO t VALUES (3, NULL, NULL)");
    db
}

/// A table with large COMPRESSIBLE values exercising Slice B's forms (large-values.md §13,
/// format.md "Large values", lz4.md): row 1's "x"-run text and 0xAB-run bytea both become 0x03
/// inline-compressed; row 2's half-filler/half-run text compresses to ~200 B — smaller than plain
/// but still over RECORD_MAX → 0x04 external-compressed (a chain carrying the COMPRESSED block);
/// row 3 stays inline-plain; row 4 is NULL/NULL. Must match the Ruby reference's COMPRESSED_TABLE.
fn compressed_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, body text, blob bytea)",
    );
    run(
        &mut db,
        &format!(
            "INSERT INTO t VALUES (1, '{}', '\\x{}')",
            "x".repeat(600),
            "ab".repeat(200)
        ),
    );
    run(
        &mut db,
        &format!(
            "INSERT INTO t VALUES (2, '{}{}', NULL)",
            filler_text(200),
            "y".repeat(200)
        ),
    );
    run(&mut db, "INSERT INTO t VALUES (3, 'tiny', '\\xcafe')");
    run(&mut db, "INSERT INTO t VALUES (4, NULL, NULL)");
    db
}

/// A table with a uuid PRIMARY KEY (the first golden with a NON-integer stored key — the
/// load-bearing §8 cross-core key-path proof) plus a nullable uuid column. Exercises the value
/// codec's fixed-16-byte uuid branch (no length prefix), the uuid key encoding (bare 16 bytes),
/// a present and a NULL uuid value, and the nil/max boundary UUIDs. Rows go in via INSERT and
/// the store sorts them into key (byte) order. Must match spec/fileformat/verify.rb's UUID_TABLE.
fn uuid_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id uuid PRIMARY KEY, ref uuid)");
    run(
        &mut db,
        "INSERT INTO t VALUES \
         ('00000000-0000-0000-0000-000000000000', '550e8400-e29b-41d4-a716-446655440000'), \
         ('550e8400-e29b-41d4-a716-446655440000', NULL), \
         ('f47ac10b-58cc-4372-a567-0e02b2c3d479', '00000000-0000-0000-0000-000000000000'), \
         ('ffffffff-ffff-ffff-ffff-ffffffffffff', 'ffffffff-ffff-ffff-ffff-ffffffffffff')",
    );
    db
}

/// A table exercising the DEFAULT column constraint on disk — the catalog flags bit2 + the
/// pre-evaluated default value (written after the typmod). Covers an int default, a text
/// default, a DEFAULT NULL, a NOT NULL column with a default, a decimal default coerced to
/// numeric(6,2), and a plain no-default column. Row 1 takes every default; row 2 provides all.
fn default_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, n i32 DEFAULT 0, note text DEFAULT 'none', \
         maybe i32 DEFAULT NULL, req i32 NOT NULL DEFAULT 7, amt numeric(6,2) DEFAULT 1.5, \
         plain i16)",
    );
    run(&mut db, "INSERT INTO t (id) VALUES (1)");
    run(
        &mut db,
        "INSERT INTO t VALUES (2, 42, 'hi', 5, 9, 2.00, 100)",
    );
    db
}

/// A table with EXPRESSION column defaults (v8) — the catalog flags bit3 (default_is_expr) + the
/// expr-text written after the typmod: a `uuid DEFAULT uuidv7()`, an `i32 DEFAULT 1 + 1`, a
/// CONSTANT default beside them (bit2), and a plain no-default column. EMPTY table — the catalog
/// encoding is the cross-core proof; the per-row evaluation is covered by the conformance corpus.
fn default_expr_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, g uuid DEFAULT uuidv7(), n i32 DEFAULT 1 + 1, \
         k i32 DEFAULT 7, plain i16)",
    );
    db
}

/// A table with a timestamp column — exercises the value codec's i64-instant branch (type
/// code 8): a positive instant, a pre-1970 negative one, a BC-era one, the ±infinity sentinels,
/// and a NULL. The literals parse to the same micros the golden stores. The PK stays i32 (a
/// timestamp PK is supported, but the value-codec branch is the point here).
fn timestamp_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, ts timestamp)");
    run(&mut db, "INSERT INTO t VALUES (1, '2024-01-01 12:00:00')");
    run(&mut db, "INSERT INTO t VALUES (2, '1969-12-31 23:59:59.5')");
    run(
        &mut db,
        "INSERT INTO t VALUES (3, '0001-01-01 00:00:00 BC')",
    );
    run(&mut db, "INSERT INTO t VALUES (4, '-infinity')");
    run(&mut db, "INSERT INTO t VALUES (5, 'infinity')");
    run(&mut db, "INSERT INTO t VALUES (6, NULL)");
    db
}

/// A table with a timestamptz column (type code 9) — the same 8-byte branch; the `+05` literal
/// normalizes to UTC before storage.
fn timestamptz_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, ts timestamptz)",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (1, '2024-01-01 12:00:00+00')",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (2, '2024-01-01 12:00:00+05')",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (3, '1969-12-31 23:59:59.5+00')",
    );
    run(&mut db, "INSERT INTO t VALUES (4, '-infinity')");
    run(&mut db, "INSERT INTO t VALUES (5, 'infinity')");
    run(&mut db, "INSERT INTO t VALUES (6, NULL)");
    db
}

/// A table with an interval column (type code 11) — the fixed 16-byte value-codec branch
/// (i32 months ‖ i32 days ‖ i64 micros). A positive multi-field value, a negative value, the
/// zero interval, a months-only `'1 mon'` vs a span-equal-but-byte-distinct `'30 days'`, and a
/// NULL. The bare-string literals adapt to the interval column. PK stays i32.
fn interval_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, d interval)");
    run(&mut db, "INSERT INTO t VALUES (1, '1 mon 2 days 03:04:05')");
    run(&mut db, "INSERT INTO t VALUES (2, '-1 day')");
    run(&mut db, "INSERT INTO t VALUES (3, '0 seconds')");
    run(&mut db, "INSERT INTO t VALUES (4, '1 mon')");
    run(&mut db, "INSERT INTO t VALUES (5, '30 days')");
    run(&mut db, "INSERT INTO t VALUES (6, NULL)");
    db
}

/// A table with a f64 column (type code 12) — the 8-byte IEEE value-codec branch. A positive
/// fraction, a negative value, +0 and -0 (the sign bit is preserved on disk — distinct bytes), both
/// infinities, a canonicalized NaN (stored as the single quiet pattern `0x7FF8…000`), a NULL, and
/// `f64::MAX` (a full mantissa). Finite values enter via bare numeric literals (decimal adaptation);
/// the specials enter via typed literals in `INSERT ... SELECT` (a VALUES slot takes only bare
/// literals this slice — float.md). PK stays i32 here so this exercises the float VALUE codec in a
/// nullable non-key column (the float PRIMARY KEY form is `float64_pk_table_db`).
fn float64_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, d f64)");
    run(&mut db, "INSERT INTO t VALUES (1, 1.5)");
    run(&mut db, "INSERT INTO t VALUES (2, -2.5)");
    run(&mut db, "INSERT INTO t VALUES (3, 0.0)");
    run(&mut db, "INSERT INTO t SELECT 4, f64 '-0'");
    run(&mut db, "INSERT INTO t SELECT 5, f64 'Infinity'");
    run(&mut db, "INSERT INTO t SELECT 6, f64 '-Infinity'");
    run(&mut db, "INSERT INTO t SELECT 7, f64 'NaN'");
    run(&mut db, "INSERT INTO t VALUES (8, NULL)");
    run(
        &mut db,
        "INSERT INTO t SELECT 9, f64 '1.7976931348623157e308'",
    );
    db
}

/// A table with a f32 column (type code 13) — the 4-byte IEEE branch. The same special-value
/// coverage as `float64_table_db` (canonicalized NaN → `0x7FC00000`) plus 100.25 (exactly
/// representable in binary32). PK stays i32.
fn float32_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, r f32)");
    run(&mut db, "INSERT INTO t VALUES (1, 1.5)");
    run(&mut db, "INSERT INTO t VALUES (2, -2.5)");
    run(&mut db, "INSERT INTO t VALUES (3, 0.0)");
    run(&mut db, "INSERT INTO t SELECT 4, f32 '-0'");
    run(&mut db, "INSERT INTO t SELECT 5, f32 'Infinity'");
    run(&mut db, "INSERT INTO t SELECT 6, f32 '-Infinity'");
    run(&mut db, "INSERT INTO t SELECT 7, f32 'NaN'");
    run(&mut db, "INSERT INTO t VALUES (8, NULL)");
    run(&mut db, "INSERT INTO t VALUES (9, 100.25)");
    db
}

/// A table with a `f64` PRIMARY KEY (the `float-order-preserving` key, encoding.md §2.8): the B-tree
/// iterates float keys in the float total order (`-Inf < finite < +Inf < NaN`; `-0 = +0`). In-contract
/// literal values only (no transcendentals), so the image is cross-core byte-identical. Specials enter
/// via `INSERT … SELECT` typed literals (a VALUES slot takes only bare literals this slice). The row
/// set matches `FLOAT64_PK_TABLE` in spec/fileformat/verify.rb; insertion order is irrelevant (the PK
/// store sorts by encoded key).
fn float64_pk_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE fk (k f64 PRIMARY KEY, v i32)");
    run(&mut db, "INSERT INTO fk VALUES (1.5, 1)");
    run(&mut db, "INSERT INTO fk SELECT f64 '-Infinity', 2");
    run(&mut db, "INSERT INTO fk VALUES (0.0, 3)");
    run(&mut db, "INSERT INTO fk SELECT f64 'NaN', 4");
    run(&mut db, "INSERT INTO fk VALUES (-1.5, 5)");
    run(&mut db, "INSERT INTO fk SELECT f64 'Infinity', 6");
    db
}

/// As `float64_pk_table_db`, for a `f32` PRIMARY KEY (the 4-byte `float-order-preserving` key §2.8).
fn float32_pk_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE fk (k f32 PRIMARY KEY, v i32)");
    run(&mut db, "INSERT INTO fk VALUES (1.5, 1)");
    run(&mut db, "INSERT INTO fk SELECT f32 '-Infinity', 2");
    run(&mut db, "INSERT INTO fk VALUES (0.0, 3)");
    run(&mut db, "INSERT INTO fk SELECT f32 'NaN', 4");
    run(&mut db, "INSERT INTO fk VALUES (-1.5, 5)");
    run(&mut db, "INSERT INTO fk SELECT f32 'Infinity', 6");
    db
}

/// A table with a date column (type code 16) — the 4-byte i32 day-count value-codec branch (the
/// same int-be-signflip body as i32). A positive date, a pre-1970 negative one, a BC-era one,
/// the −infinity/+infinity sentinels (i32::MIN/MAX), and a NULL. The bare-string literals adapt to
/// the date column. PK stays i32. (spec/design/date.md)
fn date_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, d date)");
    run(&mut db, "INSERT INTO t VALUES (1, '2024-01-15')");
    run(&mut db, "INSERT INTO t VALUES (2, '1969-12-31')");
    run(&mut db, "INSERT INTO t VALUES (3, '0044-03-15 BC')");
    run(&mut db, "INSERT INTO t VALUES (4, '-infinity')");
    run(&mut db, "INSERT INTO t VALUES (5, 'infinity')");
    run(&mut db, "INSERT INTO t VALUES (6, NULL)");
    db
}

/// A composite TYPE defined + persisted (v9) AND used by a column with stored values (S3): pins
/// the recursive value codec — the null bitmap, a present-field body, and a NULL field's
/// zero-byte omission (row 2's `zip`) — spec/design/composite.md §4.
fn composite_type_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TYPE addr AS (street text NOT NULL, zip i32)",
    );
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, home addr)");
    run(&mut db, "INSERT INTO t VALUES (1, ROW('Main', 90210))");
    run(&mut db, "INSERT INTO t VALUES (2, ROW('Oak', NULL))");
    db
}

/// A composite-TYPED column used as the PRIMARY KEY (the third container key,
/// `composite-field-slots`, encoding.md §2.15 / composite.md §6) — distinct from the multi-column
/// `composite_pk_table` (a flat tuple of scalars). The stored key is the per-field §2.2 nullable
/// slots: `0x00`‖text(street) then `0x00`‖i32(zip). Rows are INSERTed in ascending composite-key
/// order (lexicographic — street, then zip breaking the 'Main' tie); the tree shape is
/// insertion-order sensitive.
fn composite_key_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TYPE addr AS (street text NOT NULL, zip i32 NOT NULL)",
    );
    run(
        &mut db,
        "CREATE TABLE t (id i32, home addr, PRIMARY KEY (home))",
    );
    run(&mut db, "INSERT INTO t VALUES (1, ROW('', -1))");
    run(&mut db, "INSERT INTO t VALUES (2, ROW('Elm', 100))");
    run(&mut db, "INSERT INTO t VALUES (3, ROW('Main', 5))");
    run(&mut db, "INSERT INTO t VALUES (4, ROW('Main', 90210))");
    db
}

/// A composite type used as an array ELEMENT type (array-of-composite, array.md §12 AC1): the
/// catalog array-column entry carries a composite element descriptor (`element_type_code` 14 +
/// "addr") and the value body recurses (an array body whose elements are composite bodies). Row 2's
/// element has a NULL `zip` field (the composite null-bitmap inside an element); row 3 mixes a
/// present composite element with a NULL element (the array HAS_NULLS bitmap).
fn array_composite_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TYPE addr AS (street text NOT NULL, zip i32)",
    );
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, items addr[])");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, '{\"(Main,90210)\",\"(Side,5)\"}')",
    );
    run(&mut db, "INSERT INTO t VALUES (2, '{\"(Oak,)\"}')");
    run(&mut db, "INSERT INTO t VALUES (3, '{\"(A,1)\",NULL}')");
    run(&mut db, "INSERT INTO t VALUES (4, '{}')");
    run(&mut db, "INSERT INTO t VALUES (5, NULL)");
    db
}

/// A composite type with an array-typed FIELD (array.md §12 — the mirror of array-of-composite):
/// the catalog composite-type entry carries a code-15 array field (`element_type_code` 2 = i32)
/// and the value body recurses (a composite body whose `pts` field is an array body). Row 2 has an
/// empty array field `{}` (ndim 0); row 3 a NULL array field (the composite null-bitmap).
fn composite_array_field_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE poly AS (name text, pts i32[])");
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, p poly)");
    run(&mut db, "INSERT INTO t VALUES (1, ROW('a', '{10,20,30}'))");
    run(&mut db, "INSERT INTO t VALUES (2, ROW('b', '{}'))");
    run(&mut db, "INSERT INTO t VALUES (3, ROW('c', NULL))");
    db
}

/// Nested composite types (a field whose type is another composite, by name) used by a column
/// with a stored nested value (S3). `point` is created first (a referenced type must exist), but
/// the on-disk order is name-sorted (`line`, `point`) — `line` sorts BEFORE the `point` it
/// references, so the two-pass load (collect all, then resolve) is exercised; the row pins the
/// recursive value codec descending through a composite field.
fn nested_composite_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TYPE point AS (x i32 NOT NULL, y i32 NOT NULL)",
    );
    run(&mut db, "CREATE TYPE line AS (a point, b point)");
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, ln line)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, ROW(ROW(1, 2), ROW(3, 4)))",
    );
    db
}

/// Sequences (v12): two sequences — `s1` ascending, advanced 3 times (is_called, last_value 3),
/// `s2` descending/fresh with non-default cache + cycle — plus a one-row table, pinning the
/// sequence catalog entry (entry_kind 2) and the catalog emission order (sequences before tables).
fn sequence_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE SEQUENCE s1");
    run(&mut db, "SELECT nextval('s1')");
    run(&mut db, "SELECT nextval('s1')");
    run(&mut db, "SELECT nextval('s1')");
    run(
        &mut db,
        "CREATE SEQUENCE s2 INCREMENT BY -2 MINVALUE -100 MAXVALUE -1 CACHE 5 CYCLE",
    );
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
    run(&mut db, "INSERT INTO t VALUES (1, 10)");
    db
}

/// serial_table (v13): the OWNED-sequence link (the has_owner flag bit + the owner table-name/
/// column-ordinal tail). The serial column id desugars to an i32 column that is NOT NULL (via the
/// PK) with an expression DEFAULT nextval('t_id_seq'), and an OWNED sequence t_id_seq created
/// alongside; one INSERT advances it once. Must match the Ruby reference's SERIAL_TABLE
/// (spec/fileformat/verify.rb), spec/design/sequences.md §12.
fn serial_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id serial PRIMARY KEY, v text)");
    run(&mut db, "INSERT INTO t (v) VALUES ('hello')");
    db
}

/// identity_table (v15): the two IDENTITY column flag bits (bit4 is_identity, bit5 identity_always)
/// for both kinds, atop the same serial-shaped owned-sequence bytes. `id` is GENERATED ALWAYS
/// (flags bit1+bit3+bit4+bit5), `n` is GENERATED BY DEFAULT (flags bit1+bit3+bit4); each gets an
/// owned default-i64 sequence + an expression DEFAULT nextval('<seq>'). One INSERT advances both.
/// Must match the Ruby reference's IDENTITY_TABLE (spec/fileformat/verify.rb), sequences.md §13.
fn identity_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
         n int GENERATED BY DEFAULT AS IDENTITY, v text)",
    );
    run(&mut db, "INSERT INTO t (v) VALUES ('hi')");
    db
}

/// A reference-only COLLATION (v18 — entry_kind 3 metadata entry + per-column collations): the
/// vendored `unicode` collation (the real version-pinned CLDR-DUCET root, UCA/UCD 17.0.0) as the
/// per-database default (the `is_default` flag), a column with an explicit `COLLATE "unicode"` (flags
/// bit6 + name), an un-annotated column inheriting the default (bit6 + name), and an explicit
/// `COLLATE "C"` column (no collation, bit6 clear). `unicode` is NOT imported — it is vendored, and
/// its metadata entry is emitted because the schema references it. Must match the Ruby reference's
/// COLLATION_TABLE (spec/fileformat/verify.rb), spec/design/collation.md §5.
fn collation_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.set_default_collation("unicode").unwrap(); // vendored — no import
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE \"unicode\", \
         plain text, byteorder text COLLATE \"C\")",
    );
    run(&mut db, "INSERT INTO t VALUES (1, 'a', 'b', 'z')");
    run(&mut db, "INSERT INTO t VALUES (2, 'z', 'a', 'a')");
    db
}

/// A collated text PRIMARY KEY + a collated secondary index (slice 1e, encoding.md §2.12): both keys
/// store the `unicode` UCA sort key, so the B-tree iterates in collation order. `unicode` is vendored
/// (not the default; its entry is emitted because the columns reference it). Must match the Ruby
/// reference's COLLATION_PK_TABLE.
fn collation_pk_table_db() -> Session {
    let mut db = Database::create(CreateOptions {
        page_size: GOLDEN_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (name text COLLATE \"unicode\" PRIMARY KEY, tag text COLLATE \"unicode\")",
    );
    run(&mut db, "CREATE INDEX t_tag_idx ON t (tag)");
    // Inserted out of collation order; stored in collation order ('a' < 'z' by the sort key).
    run(&mut db, "INSERT INTO t VALUES ('z', 'a')");
    run(&mut db, "INSERT INTO t VALUES ('a', 'b')");
    db
}

/// WRITE side: serializing the in-memory database reproduces the golden byte-exactly.
#[test]
fn write_matches_goldens() {
    load_unicode(); // the unicode-collated goldens need the bundle loaded (collation.md §4)
    let cases: &[(&str, Builder)] = &[
        (
            "empty_db.jed",
            (|| {
                Database::create(CreateOptions::default())
                    .unwrap()
                    .session(SessionOptions::default())
            }) as Builder,
        ),
        ("overflow_table.jed", overflow_table_db),
        ("compressed_table.jed", compressed_table_db),
        ("one_table_empty.jed", one_table_empty_db),
        ("row_count_table.jed", row_count_table_db),
        ("statistics_table.jed", statistics_table_db),
        ("pk_table.jed", pk_table_db),
        ("text_table.jed", text_table_db),
        ("varchar_table.jed", varchar_table_db),
        ("bool_table.jed", bool_table_db),
        ("bool_pk_table.jed", bool_pk_table_db),
        ("decimal_table.jed", decimal_table_db),
        ("bytea_table.jed", bytea_table_db),
        ("text_pk_table.jed", text_pk_table_db),
        ("bytea_pk_table.jed", bytea_pk_table_db),
        ("decimal_pk_table.jed", decimal_pk_table_db),
        ("uuid_table.jed", uuid_table_db),
        ("default_table.jed", default_table_db),
        ("default_expr_table.jed", default_expr_table_db),
        ("timestamp_table.jed", timestamp_table_db),
        ("timestamptz_table.jed", timestamptz_table_db),
        ("interval_table.jed", interval_table_db),
        ("interval_pk_table.jed", interval_pk_table_db),
        ("float64_table.jed", float64_table_db),
        ("float32_table.jed", float32_table_db),
        ("float64_pk_table.jed", float64_pk_table_db),
        ("float32_pk_table.jed", float32_pk_table_db),
        ("date_table.jed", date_table_db),
        ("nopk_table.jed", nopk_table_db),
        ("composite_pk_table.jed", composite_pk_table_db),
        ("check_table.jed", check_table_db),
        ("index_table.jed", index_table_db),
        ("unique_table.jed", unique_table_db),
        ("expr_index_table.jed", expr_index_table_db),
        ("partial_index_table.jed", partial_index_table_db),
        ("gin_array_table.jed", gin_array_table_db),
        ("gin_uuid_table.jed", gin_uuid_table_db),
        ("fk_table.jed", fk_table_db),
        ("composite_type_table.jed", composite_type_table_db),
        ("composite_key_table.jed", composite_key_table_db),
        ("nested_composite_table.jed", nested_composite_table_db),
        ("sequence_table.jed", sequence_table_db),
        ("serial_table.jed", serial_table_db),
        ("identity_table.jed", identity_table_db),
        ("array_table.jed", array_table_db),
        ("range_table.jed", range_table_db),
        ("range_pk_table.jed", range_pk_table_db),
        ("array_pk_table.jed", array_pk_table_db),
        ("gist_range_table.jed", gist_range_table_db),
        ("gist_scalar_table.jed", gist_scalar_table_db),
        ("gist_exclude_table.jed", gist_exclude_table_db),
        ("array_composite_table.jed", array_composite_table_db),
        (
            "composite_array_field_table.jed",
            composite_array_field_table_db,
        ),
        ("collation_table.jed", collation_table_db),
        ("collation_pk_table.jed", collation_pk_table_db),
        ("json_table.jed", json_table_db),
        ("jsonb_table.jed", jsonb_table_db),
        ("tall_tree.jed", tall_tree_db),
        ("max_sep_table.jed", max_sep_table_db),
    ];
    for (name, build) in cases {
        let image = build().to_image(GOLDEN_PAGE_SIZE, 1).unwrap();
        assert_eq!(image, fixture(name), "serialized bytes differ from {name}");
    }
}

#[test]
fn statistics_semantic_corruption_is_rejected() {
    let mut image = fixture("statistics_table.jed");
    // kind=4, summary=0, name="fresh", column=0, flags=distribution. Forge a reserved flag while
    // keeping the containing catalog page checksum valid, so this reaches the statistics decoder.
    let pattern = [4, 0, 0, 5, b'f', b'r', b'e', b's', b'h', 0, 0, 2];
    let matches: Vec<usize> = image
        .windows(pattern.len())
        .enumerate()
        .filter_map(|(at, bytes)| (bytes == pattern).then_some(at))
        .collect();
    assert_eq!(matches.len(), 1, "locate one statistics summary");
    let flags = matches[0] + pattern.len() - 1;
    image[flags] = 0x80;
    let page_start = flags / GOLDEN_PAGE_SIZE as usize * GOLDEN_PAGE_SIZE as usize;
    refresh_test_page_crc(&mut image, page_start);
    let error = match Database::from_image(&image) {
        Ok(_) => panic!("reserved statistics flag must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "XX001");
}

/// READ side: loading a golden reproduces the same rows the builder produced. The
/// torn-meta goldens must read through the valid slot to the pk_table content.
#[test]
fn read_goldens_reproduces_rows() {
    load_unicode(); // the unicode-collated goldens open via a loaded bundle (collation.md §4)
    let cases: &[(&str, Builder, &str)] = &[
        ("one_table_empty.jed", one_table_empty_db, "t"),
        ("row_count_table.jed", row_count_table_db, "t"),
        ("overflow_table.jed", overflow_table_db, "t"),
        ("compressed_table.jed", compressed_table_db, "t"),
        ("pk_table.jed", pk_table_db, "t"),
        ("text_table.jed", text_table_db, "t"),
        ("bool_table.jed", bool_table_db, "t"),
        ("bool_pk_table.jed", bool_pk_table_db, "t"),
        ("decimal_table.jed", decimal_table_db, "t"),
        ("bytea_table.jed", bytea_table_db, "t"),
        ("text_pk_table.jed", text_pk_table_db, "t"),
        ("bytea_pk_table.jed", bytea_pk_table_db, "t"),
        ("decimal_pk_table.jed", decimal_pk_table_db, "t"),
        ("uuid_table.jed", uuid_table_db, "t"),
        ("default_table.jed", default_table_db, "t"),
        ("default_expr_table.jed", default_expr_table_db, "t"),
        ("timestamp_table.jed", timestamp_table_db, "t"),
        ("timestamptz_table.jed", timestamptz_table_db, "t"),
        ("interval_table.jed", interval_table_db, "t"),
        ("interval_pk_table.jed", interval_pk_table_db, "t"),
        ("float64_table.jed", float64_table_db, "t"),
        ("float32_table.jed", float32_table_db, "t"),
        ("float64_pk_table.jed", float64_pk_table_db, "fk"),
        ("float32_pk_table.jed", float32_pk_table_db, "fk"),
        ("date_table.jed", date_table_db, "t"),
        ("nopk_table.jed", nopk_table_db, "r"),
        ("composite_pk_table.jed", composite_pk_table_db, "t"),
        ("check_table.jed", check_table_db, "t"),
        ("index_table.jed", index_table_db, "t"),
        ("unique_table.jed", unique_table_db, "t"),
        ("expr_index_table.jed", expr_index_table_db, "t"),
        ("partial_index_table.jed", partial_index_table_db, "t"),
        ("gin_array_table.jed", gin_array_table_db, "t"),
        ("gin_uuid_table.jed", gin_uuid_table_db, "t"),
        ("fk_table.jed", fk_table_db, "c"),
        ("composite_type_table.jed", composite_type_table_db, "t"),
        ("composite_key_table.jed", composite_key_table_db, "t"),
        ("nested_composite_table.jed", nested_composite_table_db, "t"),
        ("array_table.jed", array_table_db, "t"),
        ("range_table.jed", range_table_db, "t"),
        ("range_pk_table.jed", range_pk_table_db, "t"),
        ("array_pk_table.jed", array_pk_table_db, "k"),
        ("gist_range_table.jed", gist_range_table_db, "t"),
        ("gist_scalar_table.jed", gist_scalar_table_db, "t"),
        ("gist_exclude_table.jed", gist_exclude_table_db, "booking"),
        ("array_composite_table.jed", array_composite_table_db, "t"),
        (
            "composite_array_field_table.jed",
            composite_array_field_table_db,
            "t",
        ),
        ("collation_table.jed", collation_table_db, "t"),
        ("collation_pk_table.jed", collation_pk_table_db, "t"),
        ("json_table.jed", json_table_db, "t"),
        ("jsonb_table.jed", jsonb_table_db, "t"),
        ("serial_table.jed", serial_table_db, "t"),
        ("identity_table.jed", identity_table_db, "t"),
        ("tall_tree.jed", tall_tree_db, "t"),
        ("max_sep_table.jed", max_sep_table_db, "m"),
        ("torn_meta_slot0.jed", pk_table_db, "t"),
        ("torn_meta_slot1.jed", pk_table_db, "t"),
    ];
    for (name, build, table) in cases {
        let loaded = Database::from_image(&fixture(name))
            .unwrap_or_else(|e| panic!("load {name}: {}", e.message))
            .session(SessionOptions::default());
        let expected = build();
        assert_eq!(
            loaded.rows_in_key_order(table),
            expected.rows_in_key_order(table),
            "rows from {name} differ",
        );
    }

    // Empty database: zero tables, and a missing table reads as None.
    let empty = Database::from_image(&fixture("empty_db.jed"))
        .unwrap()
        .session(SessionOptions::default());
    assert!(empty.table("t").is_none());
}

/// READ side, catalog detail: column names, types, and flags survive exactly (a read
/// bug in an unexercised flag would otherwise slip past a rows-only check).
#[test]
fn read_golden_reconstructs_catalog() {
    let loaded = Database::from_image(&fixture("pk_table.jed"))
        .unwrap()
        .session(SessionOptions::default());
    let t = loaded.table("t").expect("table t");
    assert_eq!(t.name, "t");
    assert_eq!(t.columns.len(), 2);

    assert_eq!(t.columns[0].name, "id");
    assert_eq!(t.columns[0].ty.scalar(), ScalarType::Int32);
    assert!(t.columns[0].primary_key);
    assert!(t.columns[0].not_null);

    assert_eq!(t.columns[1].name, "v");
    assert_eq!(t.columns[1].ty.scalar(), ScalarType::Int16);
    assert!(!t.columns[1].primary_key);
    assert!(!t.columns[1].not_null);

    // A NULL value round-trips (id 3's v).
    let rows = loaded.rows_in_key_order("t").unwrap();
    assert_eq!(rows[2], vec![Value::Int(3), Value::Null]);
}

/// A column DEFAULT survives serialize→load: after loading the golden, a fresh INSERT that
/// omits the defaulted columns applies the *persisted* defaults — proving the default value
/// (not just its byte length) round-trips through the catalog (constraints.md §2).
#[test]
fn default_survives_load() {
    let mut loaded = Database::from_image(&fixture("default_table.jed"))
        .unwrap()
        .session(SessionOptions::default());
    run(&mut loaded, "INSERT INTO t (id) VALUES (3)");
    let rows = loaded.rows_in_key_order("t").unwrap();
    let last = rows.last().expect("a row");
    // id=3 (last in key order) takes every persisted default: n=0, note='none', maybe=NULL,
    // req=7, plain=NULL (and amt=1.50, not asserted here).
    assert_eq!(last[0], Value::Int(3));
    assert_eq!(last[1], Value::Int(0));
    assert_eq!(last[2], Value::Text("none".to_string()));
    assert_eq!(last[3], Value::Null);
    assert_eq!(last[4], Value::Int(7));
    assert_eq!(last[6], Value::Null);
}

/// A no-PK table's monotonic rowid counter must be reconstructed on load, so inserts
/// after a load don't collide with persisted rowids (the step-6 mutation fix).
#[test]
fn rowid_counter_survives_serialize_and_load() {
    let db = nopk_table_db(); // existing rows take rowids 0, 1, 2 (built at GOLDEN_PAGE_SIZE)
    let image = db.to_image(GOLDEN_PAGE_SIZE, 1).unwrap();
    let mut loaded = Database::from_image(&image)
        .unwrap()
        .session(SessionOptions::default());
    // The next insert must get rowid 3, not 0 — otherwise it collides (23505).
    loaded
        .query_outcome("INSERT INTO r VALUES (10, 100)", &[])
        .expect("insert after load");
    assert_eq!(loaded.rows_in_key_order("r").unwrap().len(), 4);
}

/// The default 8 KiB page size also round-trips (goldens stay at 256 for reviewable hex, but the
/// real default must work too). Built at 8192 so the in-memory tree is sized for it (the
/// page-backed B-tree's fan-out tracks the page size — spec/fileformat/format.md).
#[test]
fn round_trip_at_default_page_size() {
    let mut db = Database::create(CreateOptions {
        page_size: 8192,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i16)");
    for i in 1..=20i64 {
        let v = if i == 3 {
            "NULL".to_string()
        } else {
            (i * 10).to_string()
        };
        run(&mut db, &format!("INSERT INTO t VALUES ({i}, {v})"));
    }
    let image = db.to_image(8192, 1).unwrap();
    let loaded = Database::from_image(&image)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        loaded.rows_in_key_order("t"),
        db.rows_in_key_order("t"),
        "8 KiB round trip preserves rows",
    );
    // Re-serializing the loaded database yields identical bytes (determinism).
    assert_eq!(loaded.to_image(8192, 1).unwrap(), image);
}
