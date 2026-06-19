# frozen_string_literal: true

# scripts/reduce_selftest.rb — guards scripts/reduce.rb against rotting (run by `rake ci`).
#
# Deterministic, oracle-free, rust-only. Builds a hand-crafted multi-record `.test` with a KNOWN
# minimal failing core, runs the reducer, and asserts it distilled exactly that core. The fixture
# exercises the three behaviours that make the reducer correct (conformance.md §8):
#   - two PASSING query records (one before, one after the failure) are REMOVED;
#   - the state-changing UPDATE the failing query reads through is RETAINED (dropping it changes
#     `actual:`, so the strict-signature oracle rejects the removal);
#   - the CREATE + INSERT prerequisites are RETAINED (dropping either changes the failure).
# Net: 5 records → the minimal {CREATE, INSERT, UPDATE, failing query} = 4.

require "open3"
require "tmpdir"

REPO = File.expand_path("..", __dir__)

# The failing query reads `v` after the UPDATE (so it sees 99); its expected is deliberately wrong.
FIXTURE = <<~TEST
  # requires: ddl.create_table, ddl.primary_key, dml.insert, dml.insert_multi_row, dml.update, query.select, query.where_eq, query.order_by, types.i32

  statement ok
  CREATE TABLE t (id i32 PRIMARY KEY, v i32)

  statement ok
  INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)

  query II rowsort
  SELECT id, v FROM t WHERE id = 1
  ----
  1
  10

  statement ok
  UPDATE t SET v = 99 WHERE id = 2

  query I nosort
  SELECT v FROM t WHERE id = 2
  ----
  12345

  query II rowsort
  SELECT id, v FROM t WHERE id = 3
  ----
  3
  30
TEST

def fail(msg)
  warn "reduce_selftest: FAIL — #{msg}"
  exit 1
end

Dir.mktmpdir("reduce_selftest") do |tmp|
  src = File.join(tmp, "fixture.test")
  min = File.join(tmp, "min.test")
  File.write(src, FIXTURE)

  out, status = Open3.capture2e(
    RbConfig.ruby, File.join(REPO, "scripts/reduce.rb"), src, "--core", "rust", "-o", min
  )
  fail "reducer exited #{status.exitstatus}\n#{out}" unless status.success?
  fail "reducer wrote no output file" unless File.file?(min)

  text = File.read(min)
  # Records = blank-line-separated paragraphs after the `# requires:` header paragraph.
  paras = text.split(/\n[ \t]*\n+/).map(&:strip).reject(&:empty?)
  records = paras.drop(1) # paras[0] is the header (carries `# requires:`)

  checks = {
    "reduced to 4 records (got #{records.size})" => records.size == 4,
    "retained CREATE TABLE" => text.include?("CREATE TABLE t"),
    "retained INSERT" => text.include?("INSERT INTO t"),
    "retained the state-changing UPDATE" => text.include?("UPDATE t SET v = 99"),
    "retained the failing query" => text.include?("SELECT v FROM t WHERE id = 2"),
    "removed the passing id=1 query" => !text.include?("WHERE id = 1"),
    "removed the passing id=3 query" => !text.include?("WHERE id = 3"),
    "preserved the wrong expected value" => text.include?("12345"),
  }
  checks.each { |desc, ok| fail desc unless ok }

  puts "reduce_selftest: OK — 5 records → 4 (minimal {CREATE, INSERT, UPDATE, failing query}); " \
       "unrelated records removed, dependencies retained."
end
