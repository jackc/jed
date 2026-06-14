//! The `float32` / `float64` IEEE 754 types, end to end through `execute`
//! (spec/design/float.md). The cross-core contract is asserted on the RENDERED output (the `R`
//! tag tolerates layout, but these finite values render identically), the total order, the trap
//! model, strict-island coercion, the casts, the canonical-order-fold SUM/AVG, and a transcendental.

use jed::{Database, Outcome, execute};

fn db_with(stmts: &[&str]) -> Database {
    let mut db = Database::new();
    for s in stmts {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn rendered(db: &mut Database, sql: &str) -> Vec<Vec<String>> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect())
            .collect(),
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

fn one(db: &mut Database, sql: &str) -> String {
    let rows = rendered(db, sql);
    assert_eq!(rows.len(), 1, "{sql:?} should return one row");
    assert_eq!(rows[0].len(), 1, "{sql:?} should return one column");
    rows[0][0].clone()
}

fn col_types(db: &mut Database, sql: &str) -> Vec<String> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { column_types, .. } => column_types,
        Outcome::Statement { .. } => panic!("expected a query for {sql:?}"),
    }
}

fn err_code(db: &mut Database, sql: &str) -> String {
    execute(db, sql)
        .err()
        .unwrap_or_else(|| panic!("{sql:?} should have failed"))
        .code()
        .to_string()
}

// ---------------------------------------------------------------------------------------------
// Names / aliases / the promotion tower
// ---------------------------------------------------------------------------------------------

#[test]
fn aliases_resolve_and_rejected_spellings_fail() {
    // `real` → float32; `float` → float64 (the single-word aliases the parser accepts; the
    // two-word `double precision` is a from_name alias but, like `timestamp without time zone`,
    // not produced by this slice's single-identifier type parser — a documented narrowing).
    // PG's float8/float4/float(p) are NOT accepted (we own our surface).
    let mut db = db_with(&[
        "CREATE TABLE t (a real, b float)",
        "INSERT INTO t VALUES (1.5, 2.5)",
    ]);
    assert_eq!(
        col_types(&mut db, "SELECT a, b FROM t"),
        vec!["float32", "float64"]
    );
    // The canonical ids resolve too.
    let mut db2 = db_with(&["CREATE TABLE t (a float32, b float64)"]);
    assert_eq!(
        col_types(&mut db2, "SELECT * FROM t"),
        vec!["float32", "float64"]
    );
    assert!(execute(&mut db, "CREATE TABLE u (x float8)").is_err());
    assert!(execute(&mut db, "CREATE TABLE u (x float4)").is_err());
    assert!(execute(&mut db, "CREATE TABLE u (x float(10))").is_err());
}

#[test]
fn mixed_width_arithmetic_promotes_to_float64() {
    let mut db = db_with(&[
        "CREATE TABLE t (f float64, g float32)",
        "INSERT INTO t VALUES (1.5, 2.5)",
    ]);
    // float32 + float64 → float64 (the tower); float32 + float32 stays float32.
    assert_eq!(col_types(&mut db, "SELECT f + g FROM t"), vec!["float64"]);
    assert_eq!(col_types(&mut db, "SELECT g + g FROM t"), vec!["float32"]);
    assert_eq!(one(&mut db, "SELECT f + g FROM t"), "4");
}

// ---------------------------------------------------------------------------------------------
// The TOTAL order: -0 = +0, NaN = NaN (TRUE), NaN largest
// ---------------------------------------------------------------------------------------------

#[test]
fn total_order_nan_equals_nan_and_neg_zero_equals_zero() {
    let mut db = Database::new();
    // NaN = NaN is TRUE (the PG float8 total order, not raw IEEE).
    assert_eq!(one(&mut db, "SELECT float 'NaN' = float 'NaN'"), "true");
    // -0 = +0.
    assert_eq!(
        one(
            &mut db,
            "SELECT CAST(-0.0 AS float64) = CAST(0.0 AS float64)"
        ),
        "true"
    );
    // NaN is the LARGEST value: NaN > +Infinity.
    assert_eq!(
        one(&mut db, "SELECT float 'NaN' > float 'Infinity'"),
        "true"
    );
    // -Infinity < every finite value.
    assert_eq!(
        one(&mut db, "SELECT float '-Infinity' < float '-1e30'"),
        "true"
    );
    // A NaN is NOT distinct from a NaN (the total order makes them one equivalence class).
    assert_eq!(
        one(
            &mut db,
            "SELECT float 'NaN' IS NOT DISTINCT FROM float 'NaN'"
        ),
        "true"
    );
}

#[test]
fn order_by_uses_total_order_nan_last() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, f float64)",
        "INSERT INTO t VALUES (1, 1.5), (2, 0.0), (3, 0.0), (4, 0.0), (5, 0.0)",
        // Stuff the specials in via UPDATE (typed-literal RHS).
        "UPDATE t SET f = float 'Infinity' WHERE id = 3",
        "UPDATE t SET f = float '-Infinity' WHERE id = 4",
        "UPDATE t SET f = float 'NaN' WHERE id = 5",
    ]);
    // ascending: -Inf < 0 < 1.5 < +Inf < NaN.
    let ids: Vec<String> = rendered(&mut db, "SELECT id FROM t ORDER BY f")
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(ids, vec!["4", "2", "1", "3", "5"]);
}

#[test]
fn distinct_and_group_by_collapse_neg_zero_and_nan() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, f float64)",
        "INSERT INTO t VALUES (1, 0.0), (2, 0.0), (3, 0.0), (4, 0.0), (5, 1.5)",
        "UPDATE t SET f = -CAST(0.0 AS float64) WHERE id = 2", // -0.0
        "UPDATE t SET f = float 'NaN' WHERE id = 3",
        "UPDATE t SET f = float 'NaN' WHERE id = 4", // a second NaN
    ]);
    // DISTINCT: {+0 (collapses -0), NaN (both collapse), 1.5} = 3 groups.
    let distinct = rendered(&mut db, "SELECT DISTINCT f FROM t ORDER BY f");
    assert_eq!(
        distinct.len(),
        3,
        "distinct collapses -0/+0 and the two NaNs"
    );
    // GROUP BY: the zero group has 2 rows (id 1,2), the NaN group has 2 (id 3,4), 1.5 has 1.
    let groups = rendered(&mut db, "SELECT f, count(*) FROM t GROUP BY f ORDER BY f");
    let counts: Vec<String> = groups.iter().map(|r| r[1].clone()).collect();
    assert_eq!(counts, vec!["2", "1", "2"]); // 0(x2) < 1.5(x1) < NaN(x2)
}

// ---------------------------------------------------------------------------------------------
// Trap model: finite overflow 22003, x/0 22012; Inf/NaN operands propagate
// ---------------------------------------------------------------------------------------------

#[test]
fn arithmetic_trap_model() {
    let mut db = db_with(&[
        "CREATE TABLE t (big float64)",
        "INSERT INTO t VALUES (1.5)",
        "UPDATE t SET big = float '1e308'",
    ]);
    // finite × finite overflow to ±Inf traps 22003 (never produces Inf).
    assert_eq!(
        err_code(&mut db, "SELECT big * CAST(10.0 AS float64) FROM t"),
        "22003"
    );
    // x / 0 traps 22012.
    assert_eq!(
        err_code(
            &mut db,
            "SELECT CAST(1.0 AS float64) / CAST(0.0 AS float64)"
        ),
        "22012"
    );
    assert_eq!(
        err_code(
            &mut db,
            "SELECT CAST(1.0 AS float64) % CAST(0.0 AS float64)"
        ),
        "22012"
    );
    // An operand already Inf/NaN PROPAGATES (no trap): Inf + 1 = Inf, Inf - Inf = NaN.
    assert_eq!(
        one(&mut db, "SELECT float 'Infinity' + CAST(1.0 AS float64)"),
        "Infinity"
    );
    assert_eq!(
        one(&mut db, "SELECT float 'Infinity' - float 'Infinity'"),
        "NaN"
    );
    assert_eq!(
        one(&mut db, "SELECT float 'NaN' * CAST(0.0 AS float64)"),
        "NaN"
    );
}

// ---------------------------------------------------------------------------------------------
// Strict island: no implicit int/decimal ↔ float (a VALUE, not a literal)
// ---------------------------------------------------------------------------------------------

#[test]
fn strict_island_rejects_cross_family_value_ops() {
    let mut db = db_with(&[
        "CREATE TABLE t (i int32, d numeric, f float64)",
        "INSERT INTO t VALUES (5, 1.5, 2.5)",
    ]);
    assert_eq!(err_code(&mut db, "SELECT i + f FROM t"), "42804");
    assert_eq!(err_code(&mut db, "SELECT d + f FROM t"), "42804");
    assert_eq!(err_code(&mut db, "SELECT i = f FROM t"), "42804");
    assert_eq!(err_code(&mut db, "SELECT d < f FROM t"), "42804");
    // A bare LITERAL, by contrast, ADAPTS to a float context (float.md §4) — not a cross-family cast.
    assert_eq!(one(&mut db, "SELECT 1 + f FROM t"), "3.5");
    assert_eq!(one(&mut db, "SELECT f * 2 FROM t"), "5");
    assert_eq!(one(&mut db, "SELECT f FROM t WHERE f = 2.5"), "2.5");
}

// ---------------------------------------------------------------------------------------------
// Casts (all explicit except float32 → float64)
// ---------------------------------------------------------------------------------------------

#[test]
fn casts_strict_and_correct() {
    let mut db = Database::new();
    // int → float (explicit), decimal → float, float64 → float32, float32 → float64.
    assert_eq!(one(&mut db, "SELECT CAST(3 AS float64)"), "3");
    assert_eq!(one(&mut db, "SELECT CAST(1.5 AS float32)"), "1.5");
    assert_eq!(
        col_types(&mut db, "SELECT CAST(CAST(1.5 AS float32) AS float64)"),
        vec!["float64"]
    );
    // float → int: round HALF AWAY FROM ZERO (jed's one mode), not half-to-even.
    assert_eq!(
        one(&mut db, "SELECT CAST(CAST(2.5 AS float64) AS int32)"),
        "3"
    );
    assert_eq!(
        one(&mut db, "SELECT CAST(CAST(-2.5 AS float64) AS int32)"),
        "-3"
    );
    assert_eq!(
        one(&mut db, "SELECT CAST(CAST(2.4 AS float64) AS int32)"),
        "2"
    );
    // float → int of NaN / ±Inf → 22003.
    assert_eq!(
        err_code(&mut db, "SELECT CAST(float 'NaN' AS int32)"),
        "22003"
    );
    assert_eq!(
        err_code(&mut db, "SELECT CAST(float 'Infinity' AS int32)"),
        "22003"
    );
    // float → int range overflow → 22003.
    assert_eq!(
        err_code(&mut db, "SELECT CAST(float '1e18' AS int32)"),
        "22003"
    );
    // float → decimal (the EXACT decimal of the binary value), and NaN/Inf → 22003.
    assert_eq!(
        one(&mut db, "SELECT CAST(CAST(1.5 AS float64) AS numeric)"),
        "1.5"
    );
    assert_eq!(
        err_code(&mut db, "SELECT CAST(float 'NaN' AS numeric)"),
        "22003"
    );
    // The EXACT expansion of a non-representable binary64: 0.1's nearest double, taken to a
    // numeric(60,55), is its true base-10 value — the unique, cross-core answer (byte-identical
    // to Go's exactDecimalFromFloat64; spec/design/float.md §6). A shortest-string conversion
    // would instead yield "0.1000...0", which would diverge across cores.
    assert_eq!(
        one(&mut db, "SELECT CAST(float64 '0.1' AS numeric(60,55))"),
        "0.1000000000000000055511151231257827021181583404541015625"
    );
    // 0.5 / 2.5 are exactly representable → their exact decimals are short.
    assert_eq!(one(&mut db, "SELECT CAST(float64 '0.5' AS numeric)"), "0.5");
    assert_eq!(one(&mut db, "SELECT CAST(float64 '2.5' AS numeric)"), "2.5");
    // 1e20 is an exact integer in binary64 → scale-0 expansion.
    assert_eq!(
        one(&mut db, "SELECT CAST(float64 '1e20' AS numeric)"),
        "100000000000000000000"
    );
    // float32 → decimal: reaches the cast losslessly widened to f64, so it is the EXACT decimal
    // of the binary32 value (the spec's float32 algorithm = same expansion at 24-bit M).
    assert_eq!(one(&mut db, "SELECT CAST(float32 '0.5' AS numeric)"), "0.5");
    assert_eq!(
        one(&mut db, "SELECT CAST(float32 '0.1' AS numeric(40,35))"),
        "0.10000000149011611938476562500000000"
    );
    assert_eq!(
        err_code(&mut db, "SELECT CAST(float32 'NaN' AS numeric)"),
        "22003"
    );
}

// ---------------------------------------------------------------------------------------------
// SUM / AVG: the order-independent canonical-order fold
// ---------------------------------------------------------------------------------------------

#[test]
fn sum_avg_canonical_fold_is_order_independent() {
    // The same multiset summed in two different ROW orders must give the BIT-IDENTICAL result (the
    // §7 canonical-order fold). The values are chosen so a NAIVE left-fold IS order-sensitive:
    // 1e16 + 1 loses the 1 in binary64, but a canonical (sorted) fold recovers it.
    let setup = || {
        db_with(&[
            "CREATE TABLE t (id int32 PRIMARY KEY, f float64)",
            // Seed zeros, then assign each row via a typed-literal UPDATE — the typed `float '…'`
            // form exercises float64's own string parse (a bare `1e16` would now also lex as a
            // decimal literal, grammar.md §14, but the typed form is what this test pins).
            "INSERT INTO t VALUES (1, 0.0), (2, 0.0), (3, 0.0), (4, 0.0)",
        ])
    };
    let mut a = setup();
    execute(&mut a, "UPDATE t SET f = float '1e16' WHERE id = 1").unwrap();
    execute(&mut a, "UPDATE t SET f = float '1.0' WHERE id = 2").unwrap();
    execute(&mut a, "UPDATE t SET f = float '-1e16' WHERE id = 3").unwrap();
    execute(&mut a, "UPDATE t SET f = float '0.5' WHERE id = 4").unwrap();

    let mut b = setup();
    execute(&mut b, "UPDATE t SET f = float '0.5' WHERE id = 1").unwrap();
    execute(&mut b, "UPDATE t SET f = float '-1e16' WHERE id = 2").unwrap();
    execute(&mut b, "UPDATE t SET f = float '1.0' WHERE id = 3").unwrap();
    execute(&mut b, "UPDATE t SET f = float '1e16' WHERE id = 4").unwrap();

    // Both summations fold in the SAME canonical (sorted) order regardless of row/storage order —
    // the core guarantee (the specific bit value depends on the sorted fold, but is IDENTICAL for
    // any input order). Here the sorted fold is -1e16, 0.5, 1, 1e16 → 0 (the small middle terms
    // are absorbed before the large ends cancel); the point is the two orders AGREE.
    let sa = one(&mut a, "SELECT sum(f) FROM t");
    let sb = one(&mut b, "SELECT sum(f) FROM t");
    assert_eq!(sa, sb, "float SUM is order-independent (canonical fold)");

    // A well-separated multiset where the sorted fold is exact: 1 + 2 + 4 = 7, both orders.
    let mut c = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, f float64)",
        "INSERT INTO t VALUES (1, 1.0), (2, 2.0), (3, 4.0)",
    ]);
    let mut d = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, f float64)",
        "INSERT INTO t VALUES (1, 4.0), (2, 1.0), (3, 2.0)",
    ]);
    assert_eq!(one(&mut c, "SELECT sum(f) FROM t"), "7");
    assert_eq!(
        one(&mut c, "SELECT sum(f) FROM t"),
        one(&mut d, "SELECT sum(f) FROM t")
    );
    assert_eq!(
        one(&mut c, "SELECT avg(f) FROM t"),
        one(&mut d, "SELECT avg(f) FROM t")
    );

    // SUM keeps the input width; AVG too.
    assert_eq!(
        col_types(&mut a, "SELECT sum(f), avg(f) FROM t"),
        vec!["float64", "float64"]
    );
}

#[test]
fn sum_avg_special_value_resolution() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, f float64)",
        "INSERT INTO t VALUES (1, 1.0), (2, 0.0), (3, 0.0), (4, 0.0)",
        "UPDATE t SET f = float 'Infinity' WHERE id = 2",
    ]);
    // +Inf present, no -Inf, no NaN → +Inf.
    assert_eq!(one(&mut db, "SELECT sum(f) FROM t"), "Infinity");
    // Add a -Inf → mixed ±Inf → NaN.
    execute(&mut db, "UPDATE t SET f = float '-Infinity' WHERE id = 3").unwrap();
    assert_eq!(one(&mut db, "SELECT sum(f) FROM t"), "NaN");
    // Any NaN dominates everything.
    execute(&mut db, "UPDATE t SET f = float 'NaN' WHERE id = 4").unwrap();
    assert_eq!(one(&mut db, "SELECT sum(f) FROM t"), "NaN");
    // Empty group → NULL.
    assert_eq!(one(&mut db, "SELECT sum(f) FROM t WHERE id > 100"), "NULL");
}

#[test]
fn min_max_use_total_order() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, f float64)",
        "INSERT INTO t VALUES (1, 1.5), (2, 0.0), (3, 0.0), (4, 0.0)",
        "UPDATE t SET f = float 'NaN' WHERE id = 2",
        "UPDATE t SET f = float '-Infinity' WHERE id = 3",
    ]);
    // MAX picks NaN (largest in the total order); MIN picks -Infinity.
    assert_eq!(one(&mut db, "SELECT max(f) FROM t"), "NaN");
    assert_eq!(one(&mut db, "SELECT min(f) FROM t"), "-Infinity");
}

// ---------------------------------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------------------------------

#[test]
fn exact_functions() {
    let mut db = Database::new();
    assert_eq!(one(&mut db, "SELECT abs(CAST(-2.5 AS float64))"), "2.5");
    assert_eq!(
        col_types(&mut db, "SELECT abs(CAST(-2.5 AS float32))"),
        vec!["float32"]
    );
    assert_eq!(one(&mut db, "SELECT ceil(CAST(1.2 AS float64))"), "2");
    assert_eq!(one(&mut db, "SELECT floor(CAST(1.8 AS float64))"), "1");
    assert_eq!(one(&mut db, "SELECT trunc(CAST(-1.8 AS float64))"), "-1");
    assert_eq!(one(&mut db, "SELECT round(CAST(2.5 AS float64))"), "3"); // half away
    assert_eq!(one(&mut db, "SELECT sqrt(CAST(4.0 AS float64))"), "2");
    // sqrt of a negative is a DOMAIN error (NaN stays input-only).
    assert_eq!(
        err_code(&mut db, "SELECT sqrt(CAST(-1.0 AS float64))"),
        "22003"
    );
}

#[test]
fn transcendental_functions() {
    let mut db = Database::new();
    // ln(e) ≈ 1, pow(2,10) = 1024 (exact here). The `R` tag tolerates a last-ULP cross-core diff;
    // these are checked numerically.
    let ln = one(&mut db, "SELECT ln(CAST(2.718281828459045 AS float64))");
    let ln_v: f64 = ln.parse().unwrap();
    assert!((ln_v - 1.0).abs() < 1e-9, "ln(e) ≈ 1, got {ln}");
    assert_eq!(
        one(
            &mut db,
            "SELECT pow(CAST(2.0 AS float64), CAST(10.0 AS float64))"
        ),
        "1024"
    );
    // ln(0) / ln(neg) trap 22003 (domain).
    assert_eq!(
        err_code(&mut db, "SELECT ln(CAST(0.0 AS float64))"),
        "22003"
    );
    assert_eq!(
        err_code(&mut db, "SELECT ln(CAST(-1.0 AS float64))"),
        "22003"
    );
}

// ---------------------------------------------------------------------------------------------
// Keys: float is not a valid PRIMARY KEY / index this slice
// ---------------------------------------------------------------------------------------------

#[test]
fn float_primary_key_and_index_rejected() {
    let mut db = Database::new();
    assert_eq!(
        err_code(&mut db, "CREATE TABLE t (id float64 PRIMARY KEY)"),
        "0A000"
    );
    assert_eq!(
        err_code(&mut db, "CREATE TABLE t (id float32 PRIMARY KEY)"),
        "0A000"
    );
    execute(&mut db, "CREATE TABLE u (id int32 PRIMARY KEY, f float64)").unwrap();
    assert_eq!(err_code(&mut db, "CREATE INDEX ix ON u (f)"), "0A000");
}

// ---------------------------------------------------------------------------------------------
// Rendering of special values
// ---------------------------------------------------------------------------------------------

#[test]
fn rendering_of_special_values() {
    let mut db = Database::new();
    assert_eq!(one(&mut db, "SELECT float 'Infinity'"), "Infinity");
    assert_eq!(one(&mut db, "SELECT float '-Infinity'"), "-Infinity");
    assert_eq!(one(&mut db, "SELECT float 'NaN'"), "NaN");
    // -0 renders -0 (a genuine float negative zero, via negation of +0).
    assert_eq!(one(&mut db, "SELECT -CAST(0.0 AS float64)"), "-0");
    // The case-insensitive special spellings all parse.
    assert_eq!(one(&mut db, "SELECT float 'inf'"), "Infinity");
    assert_eq!(one(&mut db, "SELECT float '-inf'"), "-Infinity");
    assert_eq!(one(&mut db, "SELECT float 'nan'"), "NaN");
    // Malformed → 22P02.
    assert_eq!(err_code(&mut db, "SELECT float 'not a float'"), "22P02");
}
