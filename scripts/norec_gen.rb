# frozen_string_literal: true

# scripts/norec_gen.rb — SQLancer-style metamorphic test generator (CLAUDE.md §7, TODO.md
# Phase 8: "SQLancer-style metamorphic / generative testing"). NoREC-style oracles
# (Non-optimizing Reference Engine Construction) specialized to jed's query optimizations: a
# form that triggers an optimization and a semantically-equivalent form that does NOT must agree.
# Expected rows are known BY CONSTRUCTION from the generated data — no oracle (PG or otherwise)
# is consulted. We run every generated test on all three cores, so each is checked
# METAMORPHICALLY (the two forms agree) AND DIFFERENTIALLY (the cores agree) — the latter catches
# core disagreement, the former catches a bug ALL cores share (which differential testing cannot).
#
# Relations (one scenario each — add a new scenario when you add a new optimization, §8):
#   pushdown — `pk = K` / `pk BETWEEN a AND b` seek a B-tree node; `pk + 0 = K` is a `BinaryOp`,
#              so `detect_pk_bound` (which matches only a bare `RExpr::Column`) does NOT push it
#              and it full-scans. Both must return identical rows.
#   limit    — LIMIT short-circuits the streaming scan at the window; OFFSET / the full query do
#              not. Over a total order (`ORDER BY` the unique pk) the windows must reconstruct the
#              whole — each window matches its by-construction slice.
#   join     — a constant WHERE predicate on a base relation's bare pk bounds THAT relation's scan
#              in a join (`detect_pk_bound` per relation); `pk + 0 = K` defeats it (full scan).
#              Both must return identical rows — for INNER and for a preserved-side LEFT predicate.
#   correlated — a correlated subquery whose inner pk equals an outer column (`inr.id = o.k`)
#              bounds the inner re-scan to a per-outer-row seek; `inr.id + 0 = o.k` defeats it
#              (full inner scan per outer row). Tested through EXISTS / scalar / IN; both must match.
#   index    — `v = K` on a secondary-indexed column fetches via the index tree + per-row point
#              lookups (spec/design/indexes.md §5); `v + 0 = K` is a `BinaryOp`, so the detector
#              (bare column only) does NOT use the index and it full-scans. Both must return
#              identical rows — including across UPDATE/DELETE maintenance and a NULL value (3VL).
#   tlp      — Ternary-Logic Partitioning (SQLancer): for ANY predicate p, every row is in exactly
#              one of `WHERE p` (TRUE) / `WHERE NOT p` (FALSE) / `WHERE p IS NULL` (UNKNOWN), so the
#              three partitions UNION ALL must reconstruct the whole table (and COUNT over the whole
#              = the partition counts summed). Unlike the pushdown family this is NOT an
#              optimized-vs-unoptimized pair — it is an independent oracle for the 3-valued NULL
#              logic itself (comparison-with-NULL, Kleene AND/OR/NOT, IS NULL — the §8 hotspot).
#   gin      — `tags @> Q` over a GIN-indexed array column gathers candidates via the index
#              (spec/design/gin.md §6); the equivalent `Q <@ tags` is NOT GIN-accelerated (full
#              scan). Both must return identical rows (term gather/intersection oracle).
#   gin_any  — `c = ANY(tags)` over a GIN-indexed array column gathers c's single posting list
#              (gin.md §6); the equivalent `'{c}' <@ tags` is NOT GIN-accelerated (full scan). Both
#              must return identical rows (single-term gather oracle).
#   gin_eq   — `tags = Q` over a GIN-indexed array column gathers candidates via the @>-superset
#              bound + residual = (gin.md §6); the equivalent `NOT (tags <> Q)` is NOT GIN-accelerated
#              (full scan). Both must return identical rows (exact-equality gather oracle).
#   gin_mut  — a GIN-bounded UPDATE/DELETE: `UPDATE … WHERE tags @> Q` / `DELETE … WHERE c = ANY(tags)`
#              bound their scan through the index (gin.md §6); the SAME predicates spelled `Q <@ tags`
#              / `'{c}' <@ tags` are NOT GIN-accelerated (full scan). Applied to two identically-seeded
#              tables (one bounded, one not), both reach the SAME by-construction end state (the bound
#              is transparent under mutation).
#   window   — the window frame sliding-window optimization (window.md §5.2): an explicit expanding
#              `ROWS UNBOUNDED PRECEDING..CURRENT ROW` aggregate (the sliding path) must equal the
#              DEFAULT-frame aggregate (the separate running-pass path) — distinct ids ⇒ no peers ⇒
#              the two frames coincide; the moving COUNT(*)/SUM forms (the un-fold / partial-rebuild
#              paths) must match the by-construction rows. An independent oracle for a bug all three
#              cores might share.
#
# Algebraic-equivalence oracles (like TLP, NOT optimization pairs — equivalent SPELLINGS must agree):
#   predicate   — one predicate written many logically-equivalent ways (AND/OR commutativity, Kleene
#                 De Morgan, double negation, `IN`↔OR-chain, `BETWEEN`↔`>= AND <=`) must return the
#                 same rows under 3VL — each is a different parse/eval path (desugaring, connective
#                 precedence). Independent oracle for the boolean-connective surface (§8 hotspot).
#   setop_logic — connective ↔ set operation over a unique key: `WHERE p OR q` == `(WHERE p) UNION
#                 (WHERE q)`, `WHERE p AND q` == `(WHERE p) INTERSECT (WHERE q)`, plus UNION/INTERSECT/
#                 DISTINCT idempotence + a NULL-group DISTINCT collapse. Exercises the set-op dedup
#                 path against the same logic — catches a dedup/hash bug all cores share.
#   join_comm   — INNER JOIN commutes (`a JOIN b` == `b JOIN a`) and equals the CROSS JOIN filtered by
#                 the same equality. Same projected pairs, different execution shapes (operand order,
#                 join operator vs Cartesian-product + residual filter). Independent join-semantics oracle.
#
# Determinism (CLAUDE.md §10): generation is SEEDED, so a discovered failure reduces to this exact
# deterministic .test, which then joins the corpus. The fuzzer is dev-time discovery; the emitted
# .test is the reproducible artifact — and `scripts/reduce.rb` shrinks a failing one to a minimal
# committable regression `.test` (ddmin over records, preserving the exact failure signature).
#
#   ruby scripts/norec_gen.rb [seed]            # one seed, all scenarios, run on Go + TS
#   ruby scripts/norec_gen.rb --sweep 20        # seeds 1..20 × all scenarios, all three cores
#   ruby scripts/norec_gen.rb 7 --keep          # generate seed 7 and LEAVE the files for inspection

require "open3"
require "fileutils"

# Each core ships a conformance binary that WALKS spec/conformance/suites and prints one
# `PASS|FAIL|SKIP <relpath>` line per file (identical format across cores). So we generate every
# file into the tree, then run each core ONCE — 3 harness runs total, not 3×(files).
CORES = {
  "rust" => { dir: %w[impl rust], cmd: %w[cargo run --quiet --bin conformance] },
  "go" => { dir: %w[impl go], cmd: %w[go run ./cmd/conformance] },
  "ts" => { dir: %w[impl ts], cmd: %w[npm run --silent conformance] },
}.freeze

# Per-scenario `# requires:` — the minimal capability set, so a still-incomplete core skips a
# scenario it can't run rather than failing (conformance.md §3), and each scenario gates tightly.
PUSHDOWN_REQ = %w[ddl.create_table ddl.primary_key dml.insert dml.insert_multi_row query.select
                  query.where_eq query.comparison_order query.point_lookup query.order_by
                  query.logical_connectives expr.arithmetic expr.between expr.comparison_value
                  types.i32].freeze
LIMIT_REQ = %w[ddl.create_table ddl.primary_key dml.insert dml.insert_multi_row query.select
               query.comparison_order query.order_by query.limit query.offset
               query.limit_short_circuit types.i32].freeze
JOIN_REQ = %w[ddl.create_table ddl.primary_key dml.insert dml.insert_multi_row query.select
              query.where_eq query.comparison_order query.order_by query.order_by_keys
              query.qualified_column query.join_inner query.join_left query.join_pushdown
              query.point_lookup expr.arithmetic expr.comparison_value types.i32].freeze
CORRELATED_REQ = %w[ddl.create_table ddl.primary_key dml.insert dml.insert_multi_row query.select
                    query.where_eq query.comparison_order query.order_by query.qualified_column
                    query.subquery_scalar query.subquery_in query.subquery_exists
                    query.subquery_correlated query.correlated_pushdown expr.arithmetic types.i32
                    null.three_valued].freeze
INDEX_REQ = %w[ddl.create_table ddl.primary_key ddl.secondary_index dml.insert dml.insert_multi_row
               dml.update dml.delete query.select query.where_eq query.comparison_order
               query.order_by expr.arithmetic expr.comparison_value types.i32
               null.three_valued].freeze
TLP_REQ = %w[ddl.create_table ddl.primary_key dml.insert dml.insert_multi_row query.select
             query.where_eq query.comparison_order query.order_by query.is_null
             query.logical_connectives query.union query.aggregates query.subquery_scalar
             expr.arithmetic expr.comparison_value types.i32 null.three_valued].freeze
CTE_REQ = %w[ddl.create_table ddl.primary_key dml.insert dml.insert_multi_row query.select
             query.where_eq query.comparison_order query.order_by query.cte expr.arithmetic
             expr.between expr.comparison_value types.i32].freeze
WINDOW_REQ = %w[ddl.create_table ddl.primary_key dml.insert dml.insert_multi_row query.select
                query.order_by query.window query.window_aggregate query.window_frame types.i32].freeze
GIN_REQ = %w[ddl.create_table ddl.primary_key ddl.gin_index query.gin_scan dml.insert
             dml.insert_multi_row query.select query.order_by types.i32 types.array
             func.array_containment].freeze
GIN_ANY_REQ = %w[ddl.create_table ddl.primary_key ddl.gin_index query.gin_any_eq dml.insert
                 dml.insert_multi_row query.select query.order_by types.i32 types.array
                 func.array_quantified func.array_containment].freeze
GIN_EQ_REQ = %w[ddl.create_table ddl.primary_key ddl.gin_index query.gin_array_eq dml.insert
                dml.insert_multi_row query.select query.order_by query.where_eq types.i32
                types.array].freeze
GIN_MUT_REQ = %w[ddl.create_table ddl.primary_key ddl.gin_index query.gin_mutation query.gin_any_eq
                 dml.insert dml.insert_multi_row dml.update dml.delete query.select query.where_eq
                 query.order_by types.i32 types.array func.array_containment func.array_quantified].freeze
GIST_REQ = %w[ddl.create_table ddl.primary_key ddl.gist_index query.gist_scan dml.insert
              dml.insert_multi_row query.select query.order_by types.i32 types.range
              func.range_constructors func.range_operators].freeze
GIST_SCALAR_REQ = %w[ddl.create_table ddl.primary_key ddl.gist_scalar_index query.gist_scalar_scan
                     dml.insert dml.insert_multi_row query.select query.order_by
                     query.comparison_order query.logical_connectives types.i32 query.where_eq].freeze
PREDICATE_REQ = %w[ddl.create_table ddl.primary_key dml.insert dml.insert_multi_row query.select
                   query.comparison_order query.order_by query.logical_connectives expr.between
                   expr.in_list expr.comparison_value types.i32 null.three_valued].freeze
SETOP_LOGIC_REQ = %w[ddl.create_table ddl.primary_key dml.insert dml.insert_multi_row query.select
                     query.comparison_order query.order_by query.logical_connectives query.union
                     query.intersect query.distinct expr.comparison_value types.i32
                     null.three_valued].freeze
JOIN_COMM_REQ = %w[ddl.create_table ddl.primary_key dml.insert dml.insert_multi_row query.select
                   query.comparison_order query.order_by query.order_by_keys query.qualified_column
                   query.where_eq query.join_inner query.cross_join expr.comparison_value
                   types.i32].freeze

# The default relation note describes the NoREC pair (an optimized form vs a non-optimizable
# rewrite). TLP overrides it with its own partition-reconstruction note (it is not an opt pair).
NOREC_NOTE = ["# An optimization-triggering query and a semantically-equivalent form that does not trigger it",
              "# must return identical rows on every core. Expected rows known by construction; no oracle."].freeze
TLP_NOTE = ["# Ternary-Logic Partitioning: WHERE p / WHERE NOT p / WHERE (p) IS NULL partition every row in",
            "# 3VL, so the three UNION ALL reconstruct the whole. Expected rows known by construction; no oracle."].freeze
ALGEBRA_NOTE = ["# Algebraic rewrite equivalence: logically-equivalent spellings of one predicate (commutativity,",
                "# double negation, De Morgan, IN / BETWEEN desugaring) must return identical rows under 3VL.",
                "# Expected rows known by construction (Kleene eval); no oracle."].freeze
SETOP_NOTE = ["# Kleene connective <-> set operation: WHERE p OR q == (WHERE p) UNION (WHERE q); WHERE p AND q ==",
              "# (WHERE p) INTERSECT (WHERE q), over a unique key. An independent oracle for the set-op dedup path.",
              "# Expected rows known by construction (Kleene eval); no oracle."].freeze
JOIN_COMM_NOTE = ["# Join-shape equivalence: INNER JOIN commutes (a JOIN b == b JOIN a) and equals the CROSS JOIN",
                  "# filtered by the same condition. Same projected pairs, different execution paths.",
                  "# Expected rows known by construction; no oracle."].freeze

def header(seed, requires, desc, note: NOREC_NOTE)
  ["# Metamorphic #{desc} — GENERATED by scripts/norec_gen.rb (seed #{seed}).",
   *note,
   "# requires: #{requires.join(', ')}",
   ""]
end

# Emit one `query` record. `exp` is the flat list of rendered value strings (row-major).
def q(out, coltypes, sql, exp)
  out << "query #{coltypes} nosort"
  out << sql
  out << "----"
  out.concat(exp)
  out << ""
end

def stmt(out, sql)
  out << "statement ok" << sql << ""
end

# --- scenario: primary-key pushdown (point lookup + range) ------------------------------------
def gen_pushdown(seed)
  rng = Random.new(seed)
  ids = (1..40).to_a.sample(12, random: rng).sort
  rows = ids.map { |id| [id, rng.rand(-100..100)] }
  block = ->(pred) { rows.select { |id, v| pred.call(id, v) }.flat_map { |id, v| [id.to_s, v.to_s] } }

  present = ids.sample(2, random: rng)
  absent = ((1..40).to_a - ids).sample(random: rng)
  lo, hi = ids.sample(2, random: rng).sort

  out = header(seed, PUSHDOWN_REQ, "primary-key pushdown (point lookup + range)")
  stmt(out, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
  stmt(out, "INSERT INTO t VALUES #{rows.map { |id, v| "(#{id}, #{v})" }.join(', ')}")

  pair = lambda do |title, opt, scan, exp|
    out << "# #{title}"
    out << "# pushdown (bare pk -> B-tree seek)"
    q(out, "II", "SELECT id, v FROM t WHERE #{opt} ORDER BY id", exp)
    out << "# full scan (+0 defeats pushdown) — MUST match"
    q(out, "II", "SELECT id, v FROM t WHERE #{scan} ORDER BY id", exp)
  end

  present.each do |k|
    pair.call("point lookup id = #{k} (present)", "id = #{k}", "id + 0 = #{k}",
              block.call(->(id, _v) { id == k }))
  end
  pair.call("point lookup id = #{absent} (absent -> empty)", "id = #{absent}", "id + 0 = #{absent}",
            block.call(->(id, _v) { id == absent }))
  pair.call("range #{lo}..#{hi}", "id BETWEEN #{lo} AND #{hi}", "id + 0 BETWEEN #{lo} AND #{hi}",
            block.call(->(id, _v) { id >= lo && id <= hi }))
  pair.call("range 41..50 (empty)", "id BETWEEN 41 AND 50", "id + 0 BETWEEN 41 AND 50",
            block.call(->(id, _v) { id >= 41 && id <= 50 }))

  out.join("\n") + "\n"
end

# --- scenario: LIMIT short-circuit (windows reconstruct the ordered whole) ---------------------
def gen_limit(seed)
  rng = Random.new(seed)
  ids = (1..40).to_a.sample(10, random: rng).sort
  rows = ids.map { |id| [id, rng.rand(-50..50)] }
  flat = ->(rs) { rs.flat_map { |id, v| [id.to_s, v.to_s] } }
  n = rows.size
  a = rng.rand(2..n - 2)

  out = header(seed, LIMIT_REQ, "LIMIT short-circuit (windows reconstruct the ordered whole)")
  stmt(out, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
  stmt(out, "INSERT INTO t VALUES #{rows.map { |id, v| "(#{id}, #{v})" }.join(', ')}")

  # Each window of `ORDER BY id` (a total order) must match its by-construction slice: LIMIT a
  # short-circuits the scan; OFFSET a / the full query scan further. Boundaries (0, n, >n) stress
  # the short-circuit's stop condition. `LIMIT a` ++ `OFFSET a` reconstructs the full result.
  windows = [
    ["LIMIT #{a}", rows[0, a]],
    ["LIMIT #{a} OFFSET #{a}", rows[a, a] || []],
    ["OFFSET #{a}", rows[a..] || []],
    ["LIMIT 0", []],
    ["LIMIT #{n}", rows],
    ["LIMIT #{n + 3}", rows],
    ["OFFSET #{n}", []],
    ["", rows],
  ]
  windows.each do |clause, exp|
    out << "# ORDER BY id #{clause.empty? ? '(full reference)' : clause}"
    q(out, "II", "SELECT id, v FROM t ORDER BY id #{clause}".strip, flat.call(exp))
  end

  out.join("\n") + "\n"
end

# --- scenario: JOIN base-table pk pushdown ----------------------------------------------------
def gen_join(seed)
  rng = Random.new(seed)
  # a's key domain (1..6) is WIDER than b's (1..4), so some a-rows have NO match — that is what
  # makes the LEFT-JOIN NULL-extension path reachable (and worth pushing a predicate through).
  a = (1..20).to_a.sample(6, random: rng).sort.map { |id| [id, rng.rand(1..6)] }   # (id, join key)
  b = (101..120).to_a.sample(6, random: rng).sort.map { |id| [id, rng.rand(1..4)] }

  inner = a.flat_map { |aid, ak| b.select { |_, bk| bk == ak }.map { |bid, _| [aid, bid] } }
  left = a.flat_map do |aid, ak|
    m = b.select { |_, bk| bk == ak }
    m.empty? ? [[aid, nil]] : m.map { |bid, _| [aid, bid] }
  end
  # ORDER BY a.id, b.id with NULLs LAST (the PostgreSQL model, CLAUDE.md §8).
  ord = ->(rs) { rs.sort_by { |aid, bid| [aid, bid.nil? ? 1 : 0, bid || 0] } }
  flat = ->(rs) { ord.call(rs).flat_map { |aid, bid| [aid.to_s, bid.nil? ? "NULL" : bid.to_s] } }

  ka = inner.map(&:first).uniq.sample(random: rng) || a.first.first          # an a.id WITH matches
  ka_null = (a.map(&:first) - inner.map(&:first)).sample(random: rng) || ka  # an a.id with NO match
  jb = inner.map(&:last).uniq.sample(random: rng) || b.first.first

  out = header(seed, JOIN_REQ, "JOIN base-table pk pushdown (bound a relation by its own WHERE)")
  stmt(out, "CREATE TABLE a (id i32 PRIMARY KEY, k i32)")
  stmt(out, "CREATE TABLE b (id i32 PRIMARY KEY, k i32)")
  stmt(out, "INSERT INTO a VALUES #{a.map { |id, k| "(#{id}, #{k})" }.join(', ')}")
  stmt(out, "INSERT INTO b VALUES #{b.map { |id, k| "(#{id}, #{k})" }.join(', ')}")

  jpair = lambda do |title, join, opt, scan, exp|
    out << "# #{title}"
    out << "# pushdown (bare pk bounds this relation's scan)"
    q(out, "II", "SELECT a.id, b.id FROM a #{join} b ON a.k = b.k WHERE #{opt} ORDER BY a.id, b.id", flat.call(exp))
    out << "# full scan (+0 defeats pushdown) — MUST match"
    q(out, "II", "SELECT a.id, b.id FROM a #{join} b ON a.k = b.k WHERE #{scan} ORDER BY a.id, b.id", flat.call(exp))
  end

  jpair.call("INNER, bound a by a.id = #{ka}", "JOIN", "a.id = #{ka}", "a.id + 0 = #{ka}",
             inner.select { |aid, _| aid == ka })
  jpair.call("LEFT, bound the preserved side by a.id = #{ka_null} (NULL-extension survives pushdown)",
             "LEFT JOIN", "a.id = #{ka_null}", "a.id + 0 = #{ka_null}", left.select { |aid, _| aid == ka_null })
  jpair.call("INNER, bound b by b.id = #{jb}", "JOIN", "b.id = #{jb}", "b.id + 0 = #{jb}",
             inner.select { |_, bid| bid == jb })

  out.join("\n") + "\n"
end

# --- scenario: correlated-subquery pk pushdown ------------------------------------------------
def gen_correlated(seed)
  rng = Random.new(seed)
  inr = (1..15).to_a.sample(5, random: rng).sort.map { |id| [id, rng.rand(-20..20)] }
  inr_by_id = inr.to_h
  absent_pool = (1..30).to_a - inr.map(&:first)
  o_ids = (1..7).to_a
  # Outer keys: a mix of present (matches an inner pk), absent (no match), and a NULL (3VL —
  # `inr.id = NULL` is never true, so EXISTS is false and the scalar subquery yields NULL).
  o = o_ids.each_with_index.map do |oid, i|
    k = if i == o_ids.size - 1 then nil
        elsif rng.rand < 0.6 then inr.map(&:first).sample(random: rng)
        else absent_pool.sample(random: rng)
        end
    [oid, k]
  end

  matched = ->(ok) { ok && inr_by_id.key?(ok) ? inr_by_id[ok] : nil }
  exists_flat = o.select { |_oid, ok| ok && inr_by_id.key?(ok) }.map(&:first).sort.map(&:to_s)
  scalar_flat = o.sort_by(&:first).flat_map do |oid, ok|
    v = matched.call(ok)
    [oid.to_s, v.nil? ? "NULL" : v.to_s]
  end

  out = header(seed, CORRELATED_REQ, "correlated-subquery pk pushdown (bound the inner re-scan by the outer row)")
  stmt(out, "CREATE TABLE o (id i32 PRIMARY KEY, k i32)")
  stmt(out, "CREATE TABLE inr (id i32 PRIMARY KEY, v i32)")
  stmt(out, "INSERT INTO o VALUES #{o.map { |oid, k| "(#{oid}, #{k.nil? ? 'NULL' : k})" }.join(', ')}")
  stmt(out, "INSERT INTO inr VALUES #{inr.map { |id, v| "(#{id}, #{v})" }.join(', ')}")

  cpair = lambda do |title, coltypes, opt_sql, scan_sql, exp|
    out << "# #{title}"
    out << "# pushdown (inner pk = outer col -> per-outer-row seek)"
    q(out, coltypes, opt_sql, exp)
    out << "# full inner scan per outer row (+0 defeats pushdown) — MUST match"
    q(out, coltypes, scan_sql, exp)
  end

  cpair.call("EXISTS", "I",
             "SELECT o.id FROM o WHERE EXISTS (SELECT 1 FROM inr WHERE inr.id = o.k) ORDER BY o.id",
             "SELECT o.id FROM o WHERE EXISTS (SELECT 1 FROM inr WHERE inr.id + 0 = o.k) ORDER BY o.id",
             exists_flat)
  cpair.call("scalar subquery", "II",
             "SELECT o.id, (SELECT inr.v FROM inr WHERE inr.id = o.k) FROM o ORDER BY o.id",
             "SELECT o.id, (SELECT inr.v FROM inr WHERE inr.id + 0 = o.k) FROM o ORDER BY o.id",
             scalar_flat)
  cpair.call("IN", "I",
             "SELECT o.id FROM o WHERE o.k IN (SELECT inr.id FROM inr WHERE inr.id = o.k) ORDER BY o.id",
             "SELECT o.id FROM o WHERE o.k IN (SELECT inr.id FROM inr WHERE inr.id + 0 = o.k) ORDER BY o.id",
             exists_flat)

  out.join("\n") + "\n"
end

# --- scenario: secondary-index equality bound ---------------------------------------------------
def gen_index(seed)
  rng = Random.new(seed)
  ids = (1..40).to_a.sample(12, random: rng).sort
  null_id = ids.sample(random: rng)
  # (id, v, w): v is the indexed column, drawn from a small domain so equalities admit several
  # rows; one v is NULL (an index entry equality never matches — 3VL through the index).
  rows = ids.map { |id| [id, id == null_id ? nil : rng.rand(0..4), rng.rand(-50..50)] }
  flat = ->(rs) { rs.flat_map { |id, _v, w| [id.to_s, w.to_s] } }
  with_v = ->(k) { rows.select { |_id, v, _w| v == k } }

  present = rows.map { |_id, v, _w| v }.compact.sample(random: rng) || 0
  absent = ((0..9).to_a - rows.map { |_id, v, _w| v }).sample(random: rng) || 9

  out = header(seed, INDEX_REQ, "secondary-index equality bound (index fetch vs full scan)")
  stmt(out, "CREATE TABLE t (id i32 PRIMARY KEY, v i32, w i32)")
  stmt(out, "INSERT INTO t VALUES #{rows.map { |id, v, w| "(#{id}, #{v.nil? ? 'NULL' : v}, #{w})" }.join(', ')}")
  stmt(out, "CREATE INDEX t_v_idx ON t (v)")

  ipair = lambda do |title, k, exp|
    out << "# #{title}"
    out << "# index bound (bare indexed column -> index fetch)"
    q(out, "II", "SELECT id, w FROM t WHERE v = #{k} ORDER BY id", exp)
    out << "# full scan (+0 defeats the index bound) — MUST match"
    q(out, "II", "SELECT id, w FROM t WHERE v + 0 = #{k} ORDER BY id", exp)
  end

  ipair.call("v = #{present} (present)", present, flat.call(with_v.call(present)))
  ipair.call("v = #{absent} (absent -> empty)", absent, flat.call(with_v.call(absent)))

  # Maintenance under the metamorphic relation (indexes.md §4): an UPDATE moves rows across the
  # equality and a DELETE removes some — the index fetch and the full scan must keep agreeing.
  moved = rows.reject { |_id, v, _w| v.nil? }.sample(2, random: rng).map(&:first).sort
  rows = rows.map { |id, v, w| moved.include?(id) ? [id, present, w] : [id, v, w] }
  stmt(out, "UPDATE t SET v = #{present} WHERE id = #{moved[0]} OR id = #{moved[1]}")
  ipair.call("v = #{present} after UPDATE moved ids #{moved.join(', ')} in", present,
             flat.call(with_v.call(present)))

  victim = with_v.call(present).first.first
  rows = rows.reject { |id, _v, _w| id == victim }
  stmt(out, "DELETE FROM t WHERE id = #{victim}")
  ipair.call("v = #{present} after DELETE removed id #{victim}", present,
             flat.call(with_v.call(present)))

  out.join("\n") + "\n"
end

# --- scenario: GIN-bounded scan (@> via the GIN index vs <@ full scan) ------------------------
# `col @> Q` over a GIN-indexed array column gathers candidates from the index (spec/design/gin.md
# §6); the SEMANTICALLY IDENTICAL `Q <@ col` (contained-by) is NOT GIN-accelerated (§10) and full
# scans. So the metamorphic pair is `tags @> Q` vs `Q <@ tags` — the same predicate, the bound
# taken on one side and not the other; both must return identical rows. Expected rows are known by
# construction (a row matches iff its tags contain every distinct element of Q), so no oracle is
# consulted — this catches a GIN gather/combine bug ALL cores might share, which differential
# testing alone cannot. `'{…}'::i32[]` literals pin the element type (no bare-array adaptation).
def gen_gin(seed)
  rng = Random.new(seed)
  ids = (1..40).to_a.sample(12, random: rng).sort
  null_id = ids.sample(random: rng)
  # (id, tags): a small int array from a small domain (so @> admits several rows); one row is NULL
  # and some are empty {} (a non-empty @> never matches them — they carry no term).
  rows = ids.map do |id|
    next [id, nil] if id == null_id

    [id, Array.new(rng.rand(0..3)) { rng.rand(0..4) }]
  end
  # The by-construction @> oracle: tags contains every DISTINCT element of `ks` (a NULL tags, or a
  # missing element, → not contained). Duplicates in `ks` are a SET (PG @> semantics, gin.md §2).
  matches = ->(ks) { rows.select { |_id, t| !t.nil? && ks.uniq.all? { |k| t.include?(k) } }.map { |id, _| id.to_s } }

  elems = rows.filter_map { |_id, t| t }.flatten
  present = elems.sample(random: rng) || 0
  absent = ((0..9).to_a - elems).sample(random: rng) || 9
  # A second term co-occurring with `present` in some row (so the intersection is non-empty when
  # possible); else fall back to `present` (then [present, k2].uniq is a single term — still valid).
  # The partner row must hold a DISTINCT element other than `present`: `t.size > 1` counts
  # duplicates, so a row like `{3,3,3}` would match yet `t - [present]` is empty → `k2 = nil` →
  # the malformed literal `'{3,}'` (22P02). Require `(t.uniq - [present]).any?` instead.
  partner = rows.filter_map { |_id, t| t }.find { |t| t.include?(present) && (t.uniq - [present]).any? }
  k2 = partner ? (partner.uniq - [present]).sample(random: rng) : present

  lit = ->(t) { t.nil? ? "NULL" : "'{#{t.join(',')}}'" }
  arr = ->(ks) { "'{#{ks.join(',')}}'::i32[]" }

  out = header(seed, GIN_REQ, "GIN-bounded scan (@> via the GIN index vs <@ full scan)")
  stmt(out, "CREATE TABLE t (id i32 PRIMARY KEY, tags i32[])")
  stmt(out, "INSERT INTO t VALUES #{rows.map { |id, t| "(#{id}, #{lit.call(t)})" }.join(', ')}")
  stmt(out, "CREATE INDEX t_tags_gin ON t USING gin (tags)")

  gpair = lambda do |title, ks, exp|
    out << "# #{title}"
    out << "# GIN bound (col @> const -> term gather + intersection)"
    q(out, "I", "SELECT id FROM t WHERE tags @> #{arr.call(ks)} ORDER BY id", exp)
    out << "# full scan (Q <@ col is the same predicate, not GIN-accelerated) — MUST match"
    q(out, "I", "SELECT id FROM t WHERE #{arr.call(ks)} <@ tags ORDER BY id", exp)
  end

  gpair.call("@> {#{present}} (present)", [present], matches.call([present]))
  gpair.call("@> {#{absent}} (absent -> empty)", [absent], matches.call([absent]))
  gpair.call("@> {#{present},#{k2}} (intersection)", [present, k2], matches.call([present, k2]))

  out.join("\n") + "\n"
end

# --- scenario: GiST-bounded range containment (r @> Q via the GiST index vs Q <@ r full scan) -----
# `r @> Q` over a GiST-indexed range column descends the resident R-tree to candidate rows
# (spec/design/gist.md §5; query.gist_scan); the SEMANTICALLY IDENTICAL `Q <@ r` (contained-by, the
# constant on the LEFT) is NOT GiST-accelerated (gistMatch only detects `&&` and `col @> const`) and
# full scans. So the metamorphic pair is `r @> Q` vs `Q <@ r` — both mean "r contains Q", the bound
# taken on one side and not the other; both must return identical rows. Expected rows are known by
# construction (r contains a non-empty `[qlo,qhi)` iff r is a non-empty range with r.lo <= qlo and
# qhi <= r.hi; a NULL/empty r never contains a non-empty Q), so no oracle is consulted — this catches
# a GiST consistent-descent bug ALL cores might share, which differential testing alone cannot.
def gen_gist(seed)
  rng = Random.new(seed)
  ids = (1..40).to_a.sample(12, random: rng).sort
  null_id = ids.sample(random: rng)
  # (id, r): a canonical [lo, hi) i32 range from a small domain (so @> admits several rows); one row
  # is NULL and some are empty. r is [lo, hi] (hi exclusive), :empty, or nil.
  rows = ids.map do |id|
    next [id, nil] if id == null_id
    next [id, :empty] if rng.rand(0..5).zero?

    lo = rng.rand(0..8)
    [id, [lo, lo + rng.rand(1..6)]]
  end
  # The by-construction @> oracle for a NON-empty query [qlo, qhi): r contains it iff r is a non-empty
  # range with r.lo <= qlo and qhi <= r.hi (a NULL/empty r never contains a non-empty range).
  matches = lambda do |qlo, qhi|
    rows.select { |_id, r| r.is_a?(Array) && r[0] <= qlo && qhi <= r[1] }.map { |id, _| id.to_s }
  end

  lit = ->(r) { r.nil? ? "NULL" : r == :empty ? "'empty'" : "'[#{r[0]},#{r[1]})'" }
  qr = ->(qlo, qhi) { "int4range(#{qlo}, #{qhi})" }

  out = header(seed, GIST_REQ, "GiST-bounded scan (@> via the GiST index vs <@ full scan)")
  stmt(out, "CREATE TABLE t (id i32 PRIMARY KEY, r int4range)")
  stmt(out, "INSERT INTO t VALUES #{rows.map { |id, r| "(#{id}, #{lit.call(r)})" }.join(', ')}")
  stmt(out, "CREATE INDEX t_r_gist ON t USING gist (r)")

  gpair = lambda do |title, qlo, qhi|
    exp = matches.call(qlo, qhi)
    out << "# #{title}"
    out << "# GiST bound (r @> const -> consistent-descent gather)"
    q(out, "I", "SELECT id FROM t WHERE r @> #{qr.call(qlo, qhi)} ORDER BY id", exp)
    out << "# full scan (const <@ r is the same predicate, not GiST-accelerated) — MUST match"
    q(out, "I", "SELECT id FROM t WHERE #{qr.call(qlo, qhi)} <@ r ORDER BY id", exp)
  end

  gpair.call("@> a mid singleton {3}=[3,4)", 3, 4)
  gpair.call("@> a small span [2,5)", 2, 5)
  gpair.call("@> a high span [50,60) (likely absent)", 50, 60)

  out.join("\n") + "\n"
end

# --- scenario: GiST scalar `=` gather (col = c via the GiST index vs col >= c AND col <= c full scan)
# A `scalar_col = const` over a GiST-indexed FIXED-WIDTH scalar column gathers via the scalar `=`
# opclass's resident R-tree (spec/design/gist.md §6; query.gist_scalar_scan). The SEMANTICALLY
# IDENTICAL `col >= c AND col <= c` (a range predicate over a total order, ≡ `col = c`) is NOT a `=`
# conjunct, so it takes NO GiST bound and full-scans (the column is non-PK with only a GiST index, so
# no PK / B-tree range bound applies either). The metamorphic pair is `room = c` vs
# `room >= c AND room <= c` — both mean "room equals c", the GiST `=` bound taken on one side and not
# the other; both must return identical rows. Expected rows are known by construction (a row matches
# iff its room is non-NULL and equals c), so no oracle is consulted — this catches a scalar-GiST gather
# bug ALL cores might share, which differential testing alone cannot.
def gen_gist_scalar(seed)
  rng = Random.new(seed)
  ids = (1..40).to_a.sample(12, random: rng).sort
  null_id = ids.sample(random: rng)
  # (id, room): a small room domain so `=` admits duplicates; one row is NULL.
  rows = ids.map do |id|
    next [id, nil] if id == null_id

    [id, rng.rand(0..5)]
  end
  matches = lambda do |c|
    rows.select { |_id, room| !room.nil? && room == c }.map { |id, _| id.to_s }
  end

  out = header(seed, GIST_SCALAR_REQ, "GiST scalar `=` gather (room = c via the GiST index vs the range predicate full scan)")
  stmt(out, "CREATE TABLE t (id i32 PRIMARY KEY, room i32)")
  stmt(out, "INSERT INTO t VALUES #{rows.map { |id, room| "(#{id}, #{room.nil? ? 'NULL' : room})" }.join(', ')}")
  stmt(out, "CREATE INDEX t_room_gist ON t USING gist (room)")

  epair = lambda do |title, c|
    exp = matches.call(c)
    out << "# #{title}"
    out << "# GiST `=` bound (room = const -> consistent-descent gather)"
    q(out, "I", "SELECT id FROM t WHERE room = #{c} ORDER BY id", exp)
    out << "# full scan (room >= c AND room <= c is the same predicate, not GiST-accelerated) — MUST match"
    q(out, "I", "SELECT id FROM t WHERE room >= #{c} AND room <= #{c} ORDER BY id", exp)
  end

  epair.call("= a low value (likely several rows)", 1)
  epair.call("= a mid value", 3)
  epair.call("= a high value (likely absent)", 9)

  out.join("\n") + "\n"
end

# --- scenario: GIN-bounded `= ANY` membership (k = ANY via the GIN index vs '{k}' <@ full scan) ---
# `c = ANY(col)` over a GIN-indexed array column gathers the single term c's posting list
# (spec/design/gin.md §6; query.gin_any_eq); the SEMANTICALLY IDENTICAL `'{c}' <@ col` (contained-by)
# is NOT GIN-accelerated (§10) and full scans. So the metamorphic pair is `c = ANY(tags)` vs
# `'{c}' <@ tags` — both mean "tags contains c", the bound taken on one side and not the other; both
# must return identical rows. Expected rows are known by construction (a row matches iff its tags are
# non-NULL and contain c), so no oracle is consulted — this catches a GIN single-term gather bug ALL
# cores might share, which differential testing alone cannot.
def gen_gin_any(seed)
  rng = Random.new(seed)
  ids = (1..40).to_a.sample(12, random: rng).sort
  null_id = ids.sample(random: rng)
  # (id, tags): a small int array from a small domain (so a term admits several rows); one row is
  # NULL and some are empty {} (no element → no membership). No NULL ELEMENTS are generated, so the
  # by-construction oracle below is exact for both = ANY and <@.
  rows = ids.map do |id|
    next [id, nil] if id == null_id

    [id, Array.new(rng.rand(0..3)) { rng.rand(0..4) }]
  end
  # The by-construction oracle: tags is non-NULL and contains k. `k = ANY(tags)` and `'{k}' <@ tags`
  # are both exactly this (a NULL tags → excluded; an empty/missing tags → excluded).
  matches = ->(k) { rows.select { |_id, t| !t.nil? && t.include?(k) }.map { |id, _| id.to_s } }

  elems = rows.filter_map { |_id, t| t }.flatten
  present = elems.sample(random: rng) || 0
  absent = ((0..9).to_a - elems).sample(random: rng) || 9

  out = header(seed, GIN_ANY_REQ, "GIN-bounded = ANY membership (k = ANY via the GIN index vs '{k}' <@ full scan)")
  stmt(out, "CREATE TABLE t (id i32 PRIMARY KEY, tags i32[])")
  stmt(out, "INSERT INTO t VALUES #{rows.map { |id, t| "(#{id}, #{t.nil? ? 'NULL' : "'{#{t.join(',')}}'"})" }.join(', ')}")
  stmt(out, "CREATE INDEX t_tags_gin ON t USING gin (tags)")

  gpair = lambda do |title, k, exp|
    out << "# #{title}"
    out << "# GIN bound (k = ANY(col) -> single-term posting gather)"
    q(out, "I", "SELECT id FROM t WHERE #{k} = ANY(tags) ORDER BY id", exp)
    out << "# full scan ('{k}' <@ col is the same predicate, not GIN-accelerated) — MUST match"
    q(out, "I", "SELECT id FROM t WHERE '{#{k}}'::i32[] <@ tags ORDER BY id", exp)
  end

  gpair.call("#{present} = ANY (present)", present, matches.call(present))
  gpair.call("#{absent} = ANY (absent -> empty)", absent, matches.call(absent))

  out.join("\n") + "\n"
end

# --- scenario: GIN-bounded array equality (= via the GIN index vs NOT(<>) full scan) -----------
# `col = Q` over a GIN-indexed array column gathers candidates via the @>-superset bound (Q's
# distinct non-NULL elements, since col = Q ⟹ col @> Q) + the residual = (spec/design/gin.md §6;
# query.gin_array_eq); the SEMANTICALLY IDENTICAL `NOT (col <> Q)` is NOT GIN-accelerated (gin_match
# only matches `=`, never `<>`/`NOT`) and full scans. So the metamorphic pair is `tags = Q` vs
# `NOT (tags <> Q)` — both mean "tags equals Q exactly", the bound taken on one side and not the
# other; both must return identical rows (incl. the NULL-tags row, excluded by 3VL on both sides).
# Expected rows are known by construction (a row matches iff its tags are non-NULL and EXACTLY equal
# Q — ordered, same length/elements). No NULL ELEMENTS are generated, so the oracle is exact. This
# catches a GIN equality gather/residual bug ALL cores might share, which differential testing alone
# cannot. `'{…}'::i32[]` literals pin the element type (no bare-array adaptation).
def gen_gin_eq(seed)
  rng = Random.new(seed)
  ids = (1..40).to_a.sample(12, random: rng).sort
  null_id = ids.sample(random: rng)
  # (id, tags): a small int array from a small domain; one row is NULL and some are empty {}.
  rows = ids.map do |id|
    next [id, nil] if id == null_id

    [id, Array.new(rng.rand(0..3)) { rng.rand(0..4) }]
  end
  # The by-construction oracle: tags is non-NULL and EXACTLY equals q (ordered, same length/elements).
  matches = ->(q) { rows.select { |_id, t| !t.nil? && t == q }.map { |id, _| id.to_s } }

  # A present array — an existing non-NULL non-empty row's tags (≥1 match → the GIN bound gathers).
  present = rows.filter_map { |_id, t| t }.reject(&:empty?).sample(random: rng) || [0]
  # A miss: `present` reversed (the same term set still gathers, but the residual = rejects on order)
  # when reversal differs; else `present` + an out-of-domain sentinel 9 (no row carries it → the
  # gather intersects to empty). Either way matches.call returns the by-construction expected rows.
  reordered = present.reverse == present ? present + [9] : present.reverse

  arr = ->(q) { "'{#{q.join(',')}}'::i32[]" }

  out = header(seed, GIN_EQ_REQ, "GIN-bounded array equality (= via the GIN index vs NOT(<>) full scan)")
  stmt(out, "CREATE TABLE t (id i32 PRIMARY KEY, tags i32[])")
  stmt(out, "INSERT INTO t VALUES #{rows.map { |id, t| "(#{id}, #{t.nil? ? 'NULL' : "'{#{t.join(',')}}'"})" }.join(', ')}")
  stmt(out, "CREATE INDEX t_tags_gin ON t USING gin (tags)")

  gpair = lambda do |title, qa, exp|
    out << "# #{title}"
    out << "# GIN bound (col = const -> @>-superset gather + residual =)"
    q(out, "I", "SELECT id FROM t WHERE tags = #{arr.call(qa)} ORDER BY id", exp)
    out << "# full scan (NOT(col <> const) is the same predicate, not GIN-accelerated) — MUST match"
    q(out, "I", "SELECT id FROM t WHERE NOT (tags <> #{arr.call(qa)}) ORDER BY id", exp)
  end

  gpair.call("= {#{present.join(',')}} (present)", present, matches.call(present))
  gpair.call("= {#{reordered.join(',')}} (miss)", reordered, matches.call(reordered))
  gpair.call("= {} (empty -> full-scan fallback)", [], matches.call([]))

  out.join("\n") + "\n"
end

# --- scenario: GIN-bounded UPDATE/DELETE (index-bound mutation vs <@ full-scan mutation) ---------
# A GIN-bounded UPDATE/DELETE bounds its TARGET-ROW scan through the index (spec/design/gin.md §6;
# query.gin_mutation): `UPDATE … WHERE tags @> Q` takes the @> bound, `DELETE … WHERE c = ANY(tags)`
# the single-term membership bound. The SEMANTICALLY IDENTICAL predicates spelled with `<@` —
# `Q <@ tags` (= `tags @> Q`) and `'{c}' <@ tags` (= c ∈ tags) — are NOT GIN-accelerated (gin_match
# never matches `<@`) and full-scan. So the metamorphic relation runs the SAME mutation sequence on
# two identically-seeded tables — t1 via the GIN-accelerable spellings (the new bound path), t2 via
# the `<@` spellings (the old full-scan path) — and asserts BOTH reach the SAME end state, which is
# the by-construction expected state (UPDATE m=1 on rows whose tags ⊇ Q, then DELETE rows containing
# c). This catches a GIN-bounded-mutation bug ALL cores might share (the bound admits the wrong rows,
# or maintenance corrupts state), which differential testing alone cannot. No NULL ELEMENTS are
# generated, so the oracle is exact; `'{…}'::i32[]` literals pin the element type.
def gen_gin_mutation(seed)
  rng = Random.new(seed)
  ids = (1..40).to_a.sample(12, random: rng).sort
  null_id = ids.sample(random: rng)
  # (id, tags): a small int array from a small domain; one row is NULL and some are empty {}.
  rows = ids.map do |id|
    next [id, nil] if id == null_id

    [id, Array.new(rng.rand(0..3)) { rng.rand(0..4) }]
  end

  elems = rows.filter_map { |_id, t| t }.flatten
  present = elems.sample(random: rng) || 0
  # Q = a 1- or 2-term @> query: `present` plus a co-occurring DISTINCT element when one exists
  # (so @> admits ≥1 row); else just `present` (the .uniq collapses the duplicate — still valid).
  partner = rows.filter_map { |_id, t| t }.find { |t| t.include?(present) && (t.uniq - [present]).any? }
  k2 = partner ? (partner.uniq - [present]).sample(random: rng) : present
  qa = [present, k2].uniq
  c = elems.sample(random: rng) || 0 # the = ANY membership delete term (may equal present)

  # By construction: UPDATE sets m=1 on rows whose (non-NULL) tags ⊇ Q, THEN DELETE removes rows
  # whose (non-NULL) tags contain c. Survivors keep m (1 iff they matched @> Q). NULL/empty tags
  # never match either, so those rows survive with m=0.
  contains_qa = ->(t) { !t.nil? && qa.all? { |k| t.include?(k) } }
  survivors = rows.reject { |_id, t| !t.nil? && t.include?(c) }
  expected = survivors.sort_by(&:first).flat_map { |id, t| [id.to_s, contains_qa.call(t) ? "1" : "0"] }

  arr = ->(ks) { "'{#{ks.join(',')}}'::i32[]" }
  ins = rows.map { |id, t| "(#{id}, #{t.nil? ? 'NULL' : "'{#{t.join(',')}}'"}, 0)" }.join(", ")

  out = header(seed, GIN_MUT_REQ, "GIN-bounded UPDATE/DELETE (index-bound mutation vs <@ full-scan mutation)")
  stmt(out, "CREATE TABLE t1 (id i32 PRIMARY KEY, tags i32[], m i32)")
  stmt(out, "CREATE TABLE t2 (id i32 PRIMARY KEY, tags i32[], m i32)")
  stmt(out, "INSERT INTO t1 VALUES #{ins}")
  stmt(out, "INSERT INTO t2 VALUES #{ins}")
  stmt(out, "CREATE INDEX t1_tags_gin ON t1 USING gin (tags)")
  stmt(out, "CREATE INDEX t2_tags_gin ON t2 USING gin (tags)")

  out << "# t1: GIN-bounded mutations — `tags @> Q` and `c = ANY(tags)` take the index bound"
  stmt(out, "UPDATE t1 SET m = 1 WHERE tags @> #{arr.call(qa)}")
  stmt(out, "DELETE FROM t1 WHERE #{c} = ANY(tags)")
  out << "# t2: the SAME predicates spelled with <@ — NOT GIN-accelerated (full scan)"
  stmt(out, "UPDATE t2 SET m = 1 WHERE #{arr.call(qa)} <@ tags")
  stmt(out, "DELETE FROM t2 WHERE #{arr.call([c])} <@ tags")

  out << "# both tables reach the SAME by-construction end state (the bound is transparent under mutation)"
  q(out, "II", "SELECT id, m FROM t1 ORDER BY id", expected)
  q(out, "II", "SELECT id, m FROM t2 ORDER BY id", expected)

  out.join("\n") + "\n"
end

# --- scenario: TLP (ternary-logic partitioning) -----------------------------------------------
# Kleene 3-valued helpers: Ruby `nil` is SQL UNKNOWN. A comparison with a NULL operand is UNKNOWN;
# AND/OR follow Kleene logic; NOT(UNKNOWN) is UNKNOWN. These mirror jed's PG-default 3VL exactly
# (CLAUDE.md §8) and are how the partition is computed BY CONSTRUCTION — no oracle is consulted.
def tlp_lt(x, k) = x.nil? ? nil : x < k
def tlp_gt(x, k) = x.nil? ? nil : x > k
def tlp_le(x, k) = x.nil? ? nil : x <= k
def tlp_ge(x, k) = x.nil? ? nil : x >= k
def tlp_eq(x, y) = x.nil? || y.nil? ? nil : x == y
def tlp_add(x, y) = x.nil? || y.nil? ? nil : x + y

def tlp_and(p, q)
  return false if p == false || q == false
  return nil if p.nil? || q.nil?

  true
end

def tlp_or(p, q)
  return true if p == true || q == true
  return nil if p.nil? || q.nil?

  false
end

# Ternary-Logic Partitioning (SQLancer): for ANY predicate p, every row falls into EXACTLY ONE of
# three partitions — p is TRUE (`WHERE p`), p is FALSE (`WHERE NOT p`), or p is UNKNOWN
# (`WHERE p IS NULL`). So the three partitions UNION ALL must reconstruct the unpartitioned table,
# and each partition must equal its by-construction slice. This is an independent oracle for jed's
# 3-valued NULL logic (the §8 divergence hotspot): a bug in comparison-with-NULL, the Kleene
# AND/OR/NOT connectives, or `IS NULL` shows up as a partition that does not reconstruct the whole.
def gen_tlp(seed)
  rng = Random.new(seed)
  # 10 rows; a and b each ~25% NULL, drawn from small domains so each predicate yields a mix of
  # TRUE / FALSE / UNKNOWN across the rows (all three partition arms non-trivial). id is the PK,
  # never NULL, so predicates use the nullable a/b to reach the UNKNOWN arm.
  dom_a = [-5, 0, 5, 10, 15]
  dom_b = [0, 5, 10, 20]
  rows = (1..10).map do |id|
    a = rng.rand < 0.25 ? nil : dom_a.sample(random: rng)
    b = rng.rand < 0.25 ? nil : dom_b.sample(random: rng)
    [id, a, b]
  end
  k = [0, 5, 10].sample(random: rng)
  m = [0, 5].sample(random: rng)

  lit = ->(x) { x.nil? ? "NULL" : x.to_s }
  flat = ->(rs) { rs.flat_map { |id, a, b| [id.to_s, lit.call(a), lit.call(b)] } }
  whole = rows.sort_by(&:first)

  # Each predicate: its SQL text + a 3VL evaluator over (a, b) (true / false / nil=UNKNOWN).
  preds = [
    ["a < #{k}",              ->(a, _b) { tlp_lt(a, k) }],
    ["a = b",                 ->(a, b) { tlp_eq(a, b) }],
    ["a < #{k} AND b > #{m}", ->(a, b) { tlp_and(tlp_lt(a, k), tlp_gt(b, m)) }],
    ["a < #{k} OR b > #{m}",  ->(a, b) { tlp_or(tlp_lt(a, k), tlp_gt(b, m)) }],
    ["a + b < #{k}",          ->(a, b) { (s = tlp_add(a, b)).nil? ? nil : s < k }],
  ]

  out = header(seed, TLP_REQ, "TLP ternary-logic partitioning (WHERE p / NOT p / p IS NULL reconstruct the whole)", note: TLP_NOTE)
  stmt(out, "CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32)")
  stmt(out, "INSERT INTO t VALUES #{rows.map { |id, a, b| "(#{id}, #{lit.call(a)}, #{lit.call(b)})" }.join(', ')}")

  out << "# reference: the whole table — every row falls in exactly one partition below"
  q(out, "III", "SELECT id, a, b FROM t ORDER BY id", flat.call(whole))

  preds.each do |sql, ev|
    part = ->(want) { whole.select { |_id, a, b| ev.call(a, b) == want } }
    out << "# predicate p = (#{sql}) — TRUE / FALSE / UNKNOWN partition the rows in 3VL"
    out << "# p TRUE -> WHERE p"
    q(out, "III", "SELECT id, a, b FROM t WHERE #{sql} ORDER BY id", flat.call(part.call(true)))
    out << "# p FALSE -> WHERE NOT (p)"
    q(out, "III", "SELECT id, a, b FROM t WHERE NOT (#{sql}) ORDER BY id", flat.call(part.call(false)))
    out << "# p UNKNOWN -> WHERE (p) IS NULL"
    q(out, "III", "SELECT id, a, b FROM t WHERE (#{sql}) IS NULL ORDER BY id", flat.call(part.call(nil)))
    out << "# the three partitions UNION ALL reconstruct the whole — MUST equal the reference"
    q(out, "III",
      "SELECT id, a, b FROM t WHERE #{sql} " \
      "UNION ALL SELECT id, a, b FROM t WHERE NOT (#{sql}) " \
      "UNION ALL SELECT id, a, b FROM t WHERE (#{sql}) IS NULL ORDER BY id",
      flat.call(whole))
  end

  # Aggregate TLP: COUNT over the whole equals the three partition counts summed. COUNT never
  # returns NULL, so the sub-counts add directly (SUM/MIN/MAX TLP would need a NULL-coalesce for an
  # empty partition — jed has no COALESCE yet, so that arm is deferred, conformance.md §8). Uses the
  # AND predicate (the richest 3VL shape); the combined form is three uncorrelated scalar subqueries.
  ap, = preds[2]
  total = rows.size
  count_b = rows.count { |_id, _a, b| !b.nil? }
  out << "# aggregate TLP: COUNT over the whole = the three partition counts summed (p = #{ap})"
  q(out, "I", "SELECT count(*) FROM t", [total.to_s])
  q(out, "I",
    "SELECT (SELECT count(*) FROM t WHERE #{ap}) + (SELECT count(*) FROM t WHERE NOT (#{ap})) " \
    "+ (SELECT count(*) FROM t WHERE (#{ap}) IS NULL)",
    [total.to_s])
  q(out, "I", "SELECT count(b) FROM t", [count_b.to_s])
  q(out, "I",
    "SELECT (SELECT count(b) FROM t WHERE #{ap}) + (SELECT count(b) FROM t WHERE NOT (#{ap})) " \
    "+ (SELECT count(b) FROM t WHERE (#{ap}) IS NULL)",
    [count_b.to_s])

  out.join("\n") + "\n"
end

# --- scenario: CTE inline / materialize / direct equivalence ----------------------------------
# A single-reference CTE is INLINED, a MATERIALIZED one runs once and is buffered (cte.md §3);
# both must return the SAME rows as the equivalent query written without the WITH. The predicate
# selects a by-construction subset, so the expected rows are known without an oracle.
def gen_cte(seed)
  rng = Random.new(seed)
  ids = (1..40).to_a.sample(12, random: rng).sort
  rows = ids.map { |id| [id, rng.rand(-100..100)] }
  block = ->(pred) { rows.select { |id, v| pred.call(id, v) }.flat_map { |id, v| [id.to_s, v.to_s] } }

  lo, hi = ids.sample(2, random: rng).sort
  k = rng.rand(-100..100)

  out = header(seed, CTE_REQ, "CTE inline vs materialize vs direct (all equivalent)")
  stmt(out, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
  stmt(out, "INSERT INTO t VALUES #{rows.map { |id, v| "(#{id}, #{v})" }.join(', ')}")

  # Three forms that MUST return identical rows: the direct query, a single-reference CTE (which
  # inlines), and a MATERIALIZED CTE (which buffers). Cost differs; rows do not.
  triple = lambda do |title, pred_sql, exp|
    out << "# #{title} — direct"
    q(out, "II", "SELECT id, v FROM t WHERE #{pred_sql} ORDER BY id", exp)
    out << "# same, via a single-reference CTE (inlined) — MUST match"
    q(out, "II", "WITH c AS (SELECT id, v FROM t WHERE #{pred_sql}) SELECT id, v FROM c ORDER BY id", exp)
    out << "# same, via a MATERIALIZED CTE (buffered) — MUST match"
    q(out, "II",
      "WITH c AS MATERIALIZED (SELECT id, v FROM t WHERE #{pred_sql}) SELECT id, v FROM c ORDER BY id", exp)
  end

  triple.call("range #{lo}..#{hi}", "id BETWEEN #{lo} AND #{hi}",
              block.call(->(id, _v) { id >= lo && id <= hi }))
  triple.call("v > #{k} (full scan on a non-key column)", "v > #{k}",
              block.call(->(_id, v) { v > k }))
  triple.call("empty (id BETWEEN 41 AND 50)", "id BETWEEN 41 AND 50",
              block.call(->(id, _v) { id >= 41 && id <= 50 }))

  out.join("\n") + "\n"
end

# --- scenario: window frame sliding-window optimization (sliding path vs an equivalent form) -----
# The sliding-window optimization (window.md §5.2) carries ONE accumulator across rows instead of
# re-folding each frame: an EXPANDING frame (UNBOUNDED PRECEDING) folds each row once, a MOVING
# count un-folds the rows leaving on the left. A bug shared by all three cores would survive the
# differential check, so this is an independent oracle: the explicit expanding-ROWS form (the
# sliding path) must equal the DEFAULT-frame form (the separate running-pass path) — distinct ids
# ⇒ no peers ⇒ the two frames coincide — and the moving forms must match the by-construction rows.
def gen_window_frame(seed)
  rng = Random.new(seed)
  ids = (1..40).to_a.sample(rng.rand(8..12), random: rng).sort
  vals = ids.map { rng.rand(-50..50) }
  n = ids.length

  out = header(seed, WINDOW_REQ, "window frame sliding optimization (sliding path vs running-pass / construction)")
  stmt(out, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
  stmt(out, "INSERT INTO t VALUES #{ids.each_index.map { |i| "(#{ids[i]}, #{vals[i]})" }.join(', ')}")

  # Running prefix aggregates (distinct ids ⇒ each row is its own peer ⇒ explicit ROWS ... CURRENT
  # ROW == the default RANGE frame), as the by-construction oracle for both forms of the pair.
  psum = []
  pmax = []
  s = 0
  mx = nil
  vals.each do |v|
    s += v
    mx = mx.nil? ? v : [mx, v].max
    psum << s
    pmax << mx
  end
  exp_sum = ids.each_index.flat_map { |i| [ids[i].to_s, psum[i].to_s] }
  exp_max = ids.each_index.flat_map { |i| [ids[i].to_s, pmax[i].to_s] }

  out << "# expanding SUM: explicit ROWS UNBOUNDED PRECEDING..CURRENT ROW (the sliding path)"
  q(out, "II", "SELECT id, sum(v) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS s FROM t ORDER BY id", exp_sum)
  out << "# default frame (the separate running-pass path) — MUST match"
  q(out, "II", "SELECT id, sum(v) OVER (ORDER BY id) AS s FROM t ORDER BY id", exp_sum)

  out << "# expanding MAX: explicit ROWS (the sliding path — min/max benefit from the expanding case)"
  q(out, "II", "SELECT id, max(v) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS mx FROM t ORDER BY id", exp_max)
  out << "# default frame — MUST match"
  q(out, "II", "SELECT id, max(v) OVER (ORDER BY id) AS mx FROM t ORDER BY id", exp_max)

  # Moving 3-row window (positional ROWS): count exercises the removable un-fold path, sum the
  # partial-rebuild path. Expected rows are known by construction over the sorted positions.
  mcount = []
  msum = []
  (0...n).each do |i|
    lo = [0, i - 1].max
    hi = [n - 1, i + 1].min
    mcount << (hi - lo + 1)
    msum << (lo..hi).sum { |k| vals[k] }
  end
  exp_mc = ids.each_index.flat_map { |i| [ids[i].to_s, mcount[i].to_s] }
  exp_ms = ids.each_index.flat_map { |i| [ids[i].to_s, msum[i].to_s] }

  out << "# moving COUNT(*) ROWS 1 PRECEDING..1 FOLLOWING (the sliding un-fold path) — by construction"
  q(out, "II", "SELECT id, count(*) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS c FROM t ORDER BY id", exp_mc)
  out << "# moving SUM over the same frame (the partial-rebuild path) — by construction"
  q(out, "II", "SELECT id, sum(v) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS ms FROM t ORDER BY id", exp_ms)

  out.join("\n") + "\n"
end

# --- scenario: predicate algebra (commutativity / De Morgan / double-negation / IN / BETWEEN) ----
# Algebraic-equivalence oracle (like TLP, NOT an optimization pair): one predicate written several
# logically-equivalent ways must return identical rows under 3VL. Commutativity and De Morgan are
# Kleene-valid (NOT is involutive, the distributive laws hold over UNKNOWN); `x IN (c1,c2,c3)` is
# PG-defined as the OR-chain `x=c1 OR x=c2 OR x=c3`; `x BETWEEN lo AND hi` as `x>=lo AND x<=hi`.
# Each spelling is a DIFFERENT parse/eval path (a `BinaryOp`-of-`BinaryOp` tree vs an `In`/`Between`
# node) that must agree — catching a desugaring or connective-precedence bug ALL cores might share.
# Expected rows known by construction (the same Kleene rules jed must implement); no oracle.
def gen_predicate(seed)
  rng = Random.new(seed)
  # a/b each ~25% NULL over small domains so each predicate yields a TRUE/FALSE/UNKNOWN mix; id is
  # the PK (never NULL), so the nullable a/b are what reach the UNKNOWN arm of 3VL.
  dom_a = [-5, 0, 5, 10, 15]
  dom_b = [0, 5, 10, 20]
  rows = (1..12).map do |id|
    a = rng.rand < 0.25 ? nil : dom_a.sample(random: rng)
    b = rng.rand < 0.25 ? nil : dom_b.sample(random: rng)
    [id, a, b]
  end
  k = [0, 5, 10].sample(random: rng)
  m = [0, 5].sample(random: rng)
  blo, bhi = dom_a.sample(2, random: rng).sort
  c1, c2, c3 = dom_a.sample(3, random: rng)
  whole = rows.sort_by(&:first)
  lit = ->(x) { x.nil? ? "NULL" : x.to_s }
  flat = ->(rs) { rs.flat_map { |id, a, b| [id.to_s, lit.call(a), lit.call(b)] } }

  out = header(seed, PREDICATE_REQ,
               "predicate algebra (commutativity / De Morgan / double-negation / IN / BETWEEN)", note: ALGEBRA_NOTE)
  stmt(out, "CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32)")
  stmt(out, "INSERT INTO t VALUES #{rows.map { |id, a, b| "(#{id}, #{lit.call(a)}, #{lit.call(b)})" }.join(', ')}")

  # One group = one 3VL evaluator (the by-construction TRUE set) + N equivalent SQL spellings, all
  # emitted with the SAME expected rows (the rows where the predicate is TRUE, ordered by the pk).
  group = lambda do |title, ev, forms|
    exp = flat.call(whole.select { |_id, a, b| ev.call(a, b) == true })
    out << "# #{title} — every form selects exactly the rows where p is TRUE (3VL)"
    forms.each do |label, sql|
      out << "# #{label}"
      q(out, "III", "SELECT id, a, b FROM t WHERE #{sql} ORDER BY id", exp)
    end
  end

  group.call("AND  (a < #{k}) AND (b > #{m})",
             ->(a, b) { tlp_and(tlp_lt(a, k), tlp_gt(b, m)) },
             [["p AND q", "a < #{k} AND b > #{m}"],
              ["q AND p (commute)", "b > #{m} AND a < #{k}"],
              ["NOT(NOT p OR NOT q) (De Morgan)", "NOT (NOT (a < #{k}) OR NOT (b > #{m}))"]])
  group.call("OR  (a < #{k}) OR (b > #{m})",
             ->(a, b) { tlp_or(tlp_lt(a, k), tlp_gt(b, m)) },
             [["p OR q", "a < #{k} OR b > #{m}"],
              ["q OR p (commute)", "b > #{m} OR a < #{k}"],
              ["NOT(NOT p AND NOT q) (De Morgan)", "NOT (NOT (a < #{k}) AND NOT (b > #{m}))"]])
  group.call("double negation  a = b",
             ->(a, b) { tlp_eq(a, b) },
             [["p", "a = b"], ["NOT(NOT p)", "NOT (NOT (a = b))"]])
  group.call("IN  a IN (#{c1}, #{c2}, #{c3})",
             ->(a, _b) { a.nil? ? nil : [c1, c2, c3].include?(a) },
             [["a IN (list)", "a IN (#{c1}, #{c2}, #{c3})"],
              ["OR chain", "a = #{c1} OR a = #{c2} OR a = #{c3}"]])
  group.call("BETWEEN  a BETWEEN #{blo} AND #{bhi}",
             ->(a, _b) { tlp_and(tlp_ge(a, blo), tlp_le(a, bhi)) },
             [["a BETWEEN lo AND hi", "a BETWEEN #{blo} AND #{bhi}"],
              ["a >= lo AND a <= hi", "a >= #{blo} AND a <= #{bhi}"]])

  out.join("\n") + "\n"
end

# --- scenario: Kleene connective <-> set operation (OR=UNION, AND=INTERSECT, idempotence) ---------
# An independent oracle linking the boolean-connective path to the SET-OPERATION path: over a unique
# key, `WHERE p OR q` == `(WHERE p) UNION (WHERE q)` and `WHERE p AND q` == `(WHERE p) INTERSECT
# (WHERE q)`. This holds in 3VL exactly because Kleene OR is TRUE iff some operand is TRUE and Kleene
# AND is TRUE iff both are (so the UNKNOWN rows fall out of both sides identically), and because id is
# unique so the set-op dedup cannot change a per-row OR/AND result. It exercises the UNION/INTERSECT
# hashing+dedup machinery — a separate code path from `WHERE` filtering — against the same logic, so a
# dedup bug ALL cores share surfaces here. Idempotence (p UNION p == p, p INTERSECT p == p) and a
# DISTINCT collapse round out the dedup coverage. Expected rows known by construction; no oracle.
def gen_setop_logic(seed)
  rng = Random.new(seed)
  dom_a = [-5, 0, 5, 10, 15]
  dom_b = [0, 5, 10, 20]
  rows = (1..12).map do |id|
    a = rng.rand < 0.25 ? nil : dom_a.sample(random: rng)
    b = rng.rand < 0.25 ? nil : dom_b.sample(random: rng)
    [id, a, b]
  end
  k = [0, 5, 10].sample(random: rng)
  m = [0, 5].sample(random: rng)
  psql = "a < #{k}"
  qsql = "b > #{m}"
  # The by-construction TRUE id-sets of p and q (a NULL operand => UNKNOWN => not TRUE => excluded).
  set_p = rows.select { |_id, a, _b| tlp_lt(a, k) == true }.map(&:first).sort
  set_q = rows.select { |_id, _a, b| tlp_gt(b, m) == true }.map(&:first).sort
  or_ids = (set_p | set_q).sort
  and_ids = (set_p & set_q).sort
  ids = ->(list) { list.map(&:to_s) }
  lit = ->(x) { x.nil? ? "NULL" : x.to_s }

  out = header(seed, SETOP_LOGIC_REQ,
               "Kleene connective <-> set operation (OR=UNION, AND=INTERSECT, idempotence)", note: SETOP_NOTE)
  stmt(out, "CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32)")
  stmt(out, "INSERT INTO t VALUES #{rows.map { |id, a, b| "(#{id}, #{lit.call(a)}, #{lit.call(b)})" }.join(', ')}")

  out << "# p = (#{psql}), q = (#{qsql}); id is unique, so OR<->UNION and AND<->INTERSECT agree per row"
  out << "# OR == UNION : WHERE p OR q"
  q(out, "I", "SELECT id FROM t WHERE #{psql} OR #{qsql} ORDER BY id", ids.call(or_ids))
  out << "# (WHERE p) UNION (WHERE q) — MUST match"
  q(out, "I", "SELECT id FROM t WHERE #{psql} UNION SELECT id FROM t WHERE #{qsql} ORDER BY id", ids.call(or_ids))
  out << "# AND == INTERSECT : WHERE p AND q"
  q(out, "I", "SELECT id FROM t WHERE #{psql} AND #{qsql} ORDER BY id", ids.call(and_ids))
  out << "# (WHERE p) INTERSECT (WHERE q) — MUST match"
  q(out, "I", "SELECT id FROM t WHERE #{psql} INTERSECT SELECT id FROM t WHERE #{qsql} ORDER BY id", ids.call(and_ids))
  out << "# UNION idempotence : (WHERE p) UNION (WHERE p) == WHERE p"
  q(out, "I", "SELECT id FROM t WHERE #{psql} UNION SELECT id FROM t WHERE #{psql} ORDER BY id", ids.call(set_p))
  out << "# INTERSECT idempotence : (WHERE p) INTERSECT (WHERE p) == WHERE p"
  q(out, "I", "SELECT id FROM t WHERE #{psql} INTERSECT SELECT id FROM t WHERE #{psql} ORDER BY id", ids.call(set_p))

  # DISTINCT no-op on the unique key (must not drop or alter rows), then a real dedup: DISTINCT over
  # the NON-unique nullable column a — NULL is a single group and ORDER BY a sorts it LAST (PG model).
  out << "# DISTINCT no-op on a unique key : SELECT DISTINCT id == SELECT id"
  q(out, "I", "SELECT id FROM t WHERE #{psql} ORDER BY id", ids.call(set_p))
  q(out, "I", "SELECT DISTINCT id FROM t WHERE #{psql} ORDER BY id", ids.call(set_p))
  distinct_a = rows.map { |_id, a, _b| a }.uniq
  ordered_a = distinct_a.compact.sort + (distinct_a.include?(nil) ? [nil] : [])
  out << "# DISTINCT collapse on a non-unique nullable column (NULL is one group, sorts last)"
  q(out, "I", "SELECT DISTINCT a FROM t ORDER BY a", ordered_a.map { |x| lit.call(x) })

  out.join("\n") + "\n"
end

# --- scenario: join commutativity + cross-filter equivalence -------------------------------------
# An independent oracle for join semantics (NOT an optimization pair): an INNER JOIN commutes
# (`a JOIN b ON a.k=b.k` == `b JOIN a ON a.k=b.k`) and equals the CROSS JOIN filtered by the same
# equality (`a CROSS JOIN b WHERE a.k=b.k`). All four spellings project the same (a.id, b.id) pairs
# but drive different execution shapes (the join operator, with operands swapped, vs a Cartesian
# product + a residual filter). A bug ALL cores share — a join that drops or duplicates a match, or a
# cross-product/filter mismatch — surfaces here. Expected pairs known by construction; no oracle.
def gen_join_comm(seed)
  rng = Random.new(seed)
  # a.k and b.k share the domain 1..5 so matches exist; both pks are unique => (a.id, b.id) unique.
  a = (1..20).to_a.sample(6, random: rng).sort.map { |id| [id, rng.rand(1..5)] }
  b = (101..120).to_a.sample(6, random: rng).sort.map { |id| [id, rng.rand(1..5)] }
  inner = a.flat_map { |aid, ak| b.select { |_, bk| bk == ak }.map { |bid, _| [aid, bid] } }
  exp = inner.sort_by { |aid, bid| [aid, bid] }.flat_map { |aid, bid| [aid.to_s, bid.to_s] }

  out = header(seed, JOIN_COMM_REQ, "join commutativity + cross-filter equivalence", note: JOIN_COMM_NOTE)
  stmt(out, "CREATE TABLE a (id i32 PRIMARY KEY, k i32)")
  stmt(out, "CREATE TABLE b (id i32 PRIMARY KEY, k i32)")
  stmt(out, "INSERT INTO a VALUES #{a.map { |id, k| "(#{id}, #{k})" }.join(', ')}")
  stmt(out, "INSERT INTO b VALUES #{b.map { |id, k| "(#{id}, #{k})" }.join(', ')}")

  out << "# INNER JOIN, a then b"
  q(out, "II", "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k ORDER BY a.id, b.id", exp)
  out << "# INNER JOIN, b then a (commute) — MUST match"
  q(out, "II", "SELECT a.id, b.id FROM b JOIN a ON a.k = b.k ORDER BY a.id, b.id", exp)
  out << "# CROSS JOIN + WHERE filter (Cartesian product, residual equality) — MUST match"
  q(out, "II", "SELECT a.id, b.id FROM a CROSS JOIN b WHERE a.k = b.k ORDER BY a.id, b.id", exp)
  out << "# CROSS JOIN, b then a + WHERE — MUST match"
  q(out, "II", "SELECT a.id, b.id FROM b CROSS JOIN a WHERE a.k = b.k ORDER BY a.id, b.id", exp)

  out.join("\n") + "\n"
end

SCENARIOS = {
  "pushdown" => method(:gen_pushdown),
  "limit" => method(:gen_limit),
  "join" => method(:gen_join),
  "correlated" => method(:gen_correlated),
  "index" => method(:gen_index),
  "gin" => method(:gen_gin),
  "gin_any" => method(:gen_gin_any),
  "gin_eq" => method(:gen_gin_eq),
  "gin_mut" => method(:gen_gin_mutation),
  "gist" => method(:gen_gist),
  "gist_scalar" => method(:gen_gist_scalar),
  "tlp" => method(:gen_tlp),
  "cte" => method(:gen_cte),
  "window" => method(:gen_window_frame),
  "predicate" => method(:gen_predicate),
  "setop_logic" => method(:gen_setop_logic),
  "join_comm" => method(:gen_join_comm),
}.freeze

# Run one core's harness once; return {basename => "PASS"/"FAIL"/"SKIP"} and the detail line per
# non-pass file, for every metamorphic file in the tree.
def run_core(repo, core)
  out, = Open3.capture2e(*core[:cmd], chdir: File.join(repo, *core[:dir]))
  status = {}
  detail = {}
  out.each_line do |l|
    next unless (m = l.match(%r{^(PASS|FAIL|SKIP)\s+metamorphic/(_norec_\w+\.test)}))

    status[m[2]] = m[1]
    detail[m[2]] = l.strip if m[1] != "PASS"
  end
  [status, detail]
end

# --- argument parsing -------------------------------------------------------------------------
args = ARGV.dup
keep = !args.delete("--keep").nil?
sweep = (i = args.index("--sweep")) ? args[i + 1].to_i : nil
seeds_arg = (i = args.index("--seeds")) ? args[i + 1].split(",").map(&:to_i) : nil
cores_arg = (i = args.index("--cores")) ? args[i + 1].split(",") : nil
bare = args.find { |x| x =~ /\A\d+\z/ }

seeds = seeds_arg || (sweep ? (1..sweep).to_a : [bare ? bare.to_i : 1])
# A sweep is the full differential CI check (all three cores); a single seed defaults to the fast
# dev pair (Go + TS) unless cores are named explicitly.
cores = cores_arg || ((sweep || seeds_arg) ? CORES.keys : %w[go ts])
cores.each { |c| abort "unknown core #{c.inspect} (have: #{CORES.keys.join(', ')})" unless CORES[c] }

repo = File.expand_path("..", __dir__)
dir = File.join(repo, "spec/conformance/suites/metamorphic")

# --- generate every (scenario × seed), run each core once, report -----------------------------
FileUtils.mkdir_p(dir)
specs = []
seeds.each do |s|
  SCENARIOS.each do |scn, gen|
    text = gen.call(s)
    base = "_norec_#{scn}_seed#{s}.test"
    File.write(File.join(dir, base), text)
    specs << { scn: scn, seed: s, base: base, rels: text.scan(/^query /).size }
  end
end
total_rels = specs.sum { |sp| sp[:rels] }
span = seeds.size == 1 ? "seed #{seeds.first}" : "seeds #{seeds.first}..#{seeds.last}"
puts "NoREC metamorphic sweep — #{span} × {#{SCENARIOS.keys.join(', ')}} = #{specs.size} files " \
     "(#{total_rels} relations), cores: #{cores.join(', ')}"

failures = 0
begin
  cores.each do |name|
    status, detail = run_core(repo, CORES[name])
    failed = specs.reject { |sp| status[sp[:base]] == "PASS" }
    failures += failed.size
    printf("  %-4s %3d/%-3d files PASS%s\n", name, specs.size - failed.size, specs.size,
           failed.empty? ? "" : "  — FAILED: #{failed.map { |sp| "#{sp[:scn]}@#{sp[:seed]}" }.join(', ')}")
    failed.each { |sp| puts "       #{detail[sp[:base]] || "#{sp[:base]} (not run / skipped)"}" }
  end
ensure
  unless keep
    specs.each { |sp| FileUtils.rm_f(File.join(dir, sp[:base])) }
    Dir.rmdir(dir) if Dir.exist?(dir) && Dir.empty?(dir)
  end
end

checks = specs.size * cores.size
if failures.zero?
  puts "\nsweep PASS — #{checks} checks (#{specs.size} files × #{cores.size} cores), " \
       "#{SCENARIOS.size} relations/seed, all green."
  exit 0
else
  puts "\nsweep FAIL — #{failures}/#{checks} checks failed."
  exit 1
end
