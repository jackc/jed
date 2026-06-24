# frozen_string_literal: true

require_relative "test_helper"

# Tests for the Ruby gem's binding seam (spec/design/ruby.md). These exercise the WRAP — handle
# lifecycle, value marshalling/coercion, NULL handling, error mapping, persistence — NOT SQL
# semantics, which the gem inherits from the Rust core by construction (cores.md §1). Per
# CLAUDE.md §10, SQL behavior is tested once in the shared corpus on every core; re-asserting it
# here would add no coverage and drift. So everything below is structurally out of the corpus's
# reach: the FFI boundary itself.
class DatabaseTest < Minitest::Test
  def test_abi_version_matches
    assert_equal Jed::ABI_VERSION, Jed::FFI::ABI_VERSION_FN.call
  end

  def test_memory_block_yields_and_closes
    captured = nil
    result = Jed.memory do |db|
      captured = db
      assert_instance_of Jed::Database, db
      refute db.closed?
      :return_value
    end
    assert_equal :return_value, result
    assert captured.closed?, "block form must close the handle"
  end

  def test_ddl_dml_outcome_shape
    Jed.memory do |db|
      out = db.execute("CREATE TABLE t (id i32 PRIMARY KEY, name text)")
      assert_nil out[:rows_affected], "DDL has no row count"
      assert_kind_of Integer, out[:cost]

      ins = db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')")
      assert_equal 3, ins[:rows_affected]

      upd = db.execute("UPDATE t SET name = 'z' WHERE id >= 2")
      assert_equal 2, upd[:rows_affected]

      del = db.execute("DELETE FROM t WHERE id = 1")
      assert_equal 1, del[:rows_affected]
    end
  end

  def test_query_result_columns_types_and_cost
    Jed.memory do |db|
      res = db.query("SELECT 1 AS a, 'x' AS b")
      assert_instance_of Jed::Result, res
      assert_equal %w[a b], res.columns
      assert_equal %w[i64 text], res.column_types
      assert_kind_of Integer, res.cost
      assert_equal 1, res.size
    end
  end

  def test_scalar_coercion_by_type
    Jed.memory do |db|
      row = db.query(<<~SQL).first
        SELECT
          7::i16   AS i16,
          7::i32   AS i32,
          7::i64   AS i64,
          true     AS b,
          1.5::f64 AS f,
          'hi'     AS t
      SQL
      assert_equal 7, row[:i16]
      assert_instance_of Integer, row[:i16]
      assert_equal 7, row[:i32]
      assert_equal 7, row[:i64]
      assert_equal true, row[:b]
      assert_in_delta 1.5, row[:f]
      assert_instance_of Float, row[:f]
      assert_equal "hi", row[:t]
    end
  end

  def test_float_specials
    Jed.memory do |db|
      row = db.query("SELECT 'Infinity'::f64 AS p, '-Infinity'::f64 AS n, 'NaN'::f64 AS q").first
      assert_equal Float::INFINITY, row[:p]
      assert_equal(-Float::INFINITY, row[:n])
      assert row[:q].nan?
    end
  end

  # The load-bearing reason the wire carries an explicit null flag (ruby.md §3): a SQL NULL must
  # be Ruby nil, distinct from a text value that renders as the string "NULL".
  def test_null_is_nil_distinct_from_text_null
    Jed.memory do |db|
      db.execute("CREATE TABLE t (a text, b text)")
      db.execute("INSERT INTO t VALUES (NULL, 'NULL')")
      row = db.query("SELECT a, b FROM t").first
      assert_nil row[:a]
      assert_equal "NULL", row[:b]
    end
  end

  # Types with no clean native Ruby counterpart come back as their canonical render String,
  # losslessly (ruby.md §3). (decimal/date/timestamp coercion lives in rich_types_test.rb.)
  def test_non_coerced_types_render_as_string
    Jed.memory do |db|
      row = db.query(<<~SQL).first
        SELECT
          '\\xdeadbeef'::bytea                              AS by,
          '12345678-1234-1234-1234-123456789abc'::uuid    AS u,
          INTERVAL '1 day'                                AS iv
      SQL
      assert_equal "\\xdeadbeef", row[:by]
      assert_equal "12345678-1234-1234-1234-123456789abc", row[:u]
      assert_equal "1 day", row[:iv]
    end
  end

  def test_row_access_by_index_name_symbol_and_to_h
    Jed.memory do |db|
      row = db.query("SELECT 10 AS id, 'q' AS name").first
      assert_equal 10, row[0]
      assert_equal 10, row[:id]
      assert_equal 10, row["id"]
      assert_equal "q", row[1]
      assert_equal({ "id" => 10, "name" => "q" }, row.to_h)
      assert_equal [10, "q"], row.to_a
    end
  end

  def test_result_is_enumerable
    Jed.memory do |db|
      db.execute("CREATE TABLE t (id i32 PRIMARY KEY)")
      db.execute("INSERT INTO t VALUES (1), (2), (3)")
      res = db.query("SELECT id FROM t ORDER BY id")
      assert_equal [1, 2, 3], res.map { |r| r[:id] }
      assert_equal [[1], [2], [3]], res.values
    end
  end

  def test_engine_error_maps_to_exception_with_sqlstate
    Jed.memory do |db|
      err = assert_raises(Jed::Error) { db.execute("SELECT * FROM missing") }
      assert_equal "42P01", err.sqlstate
      assert_includes err.message, "42P01"

      syntax = assert_raises(Jed::Error) { db.execute("SELORCT 1") }
      assert_equal "42601", syntax.sqlstate
    end
  end

  def test_unique_violation_sqlstate
    Jed.memory do |db|
      db.execute("CREATE TABLE u (id i32 PRIMARY KEY)")
      db.execute("INSERT INTO u VALUES (1)")
      err = assert_raises(Jed::Error) { db.execute("INSERT INTO u VALUES (1)") }
      assert_equal "23505", err.sqlstate
    end
  end

  def test_query_on_non_query_raises
    Jed.memory do |db|
      db.execute("CREATE TABLE t (id i32 PRIMARY KEY)")
      err = assert_raises(Jed::Error) { db.query("INSERT INTO t VALUES (1)") }
      assert_equal "42601", err.sqlstate
    end
  end

  def test_persistence_round_trip
    Dir.mktmpdir do |dir|
      path = File.join(dir, "db.jed")
      Jed.create(path) do |db|
        db.execute("CREATE TABLE kv (k i32 PRIMARY KEY, v text)")
        db.execute("INSERT INTO kv VALUES (1, 'one'), (2, 'two')")
        db.commit
      end
      Jed.open(path) do |db|
        assert_equal 2, db.query("SELECT count(*) AS n FROM kv").first[:n]
        assert_equal "two", db.query("SELECT v FROM kv WHERE k = 2").first[:v]
      end
    end
  end

  # jed is autocommit (CLAUDE.md §3, transactions.md): each `execute` is durable on its own — no
  # explicit commit needed. Confirm that holds through the wrap (the row survives a close+reopen
  # with no `commit` call).
  def test_autocommit_persists_each_statement
    Dir.mktmpdir do |dir|
      path = File.join(dir, "db.jed")
      Jed.create(path) do |db|
        db.execute("CREATE TABLE t (id i32 PRIMARY KEY)")
        db.execute("INSERT INTO t VALUES (42)") # no explicit commit
      end
      Jed.open(path) do |db|
        assert_equal 1, db.query("SELECT count(*) AS n FROM t").first[:n]
      end
    end
  end

  # An explicit BEGIN/ROLLBACK block runs through the wrap because the handle keeps transaction
  # state across `execute` calls; ROLLBACK discards the block's writes.
  def test_explicit_transaction_rollback_discards
    Jed.memory do |db|
      db.execute("CREATE TABLE t (id i32 PRIMARY KEY)")
      db.execute("BEGIN")
      db.execute("INSERT INTO t VALUES (1)")
      db.execute("ROLLBACK")
      assert_equal 0, db.query("SELECT count(*) AS n FROM t").first[:n]
    end
  end

  def test_read_only_rejects_writes
    Dir.mktmpdir do |dir|
      path = File.join(dir, "db.jed")
      Jed.create(path) { |db| db.execute("CREATE TABLE t (id i32 PRIMARY KEY)") && db.commit }
      Jed.open(path, read_only: true) do |db|
        err = assert_raises(Jed::Error) { db.execute("INSERT INTO t VALUES (1)") }
        assert_equal "25006", err.sqlstate
      end
    end
  end

  def test_create_over_existing_file_is_58P02
    Dir.mktmpdir do |dir|
      path = File.join(dir, "db.jed")
      Jed.create(path) { |db| db.commit }
      err = assert_raises(Jed::Error) { Jed.create(path) }
      assert_equal "58P02", err.sqlstate
    end
  end

  def test_open_missing_file_is_58P01
    Dir.mktmpdir do |dir|
      err = assert_raises(Jed::Error) { Jed.open(File.join(dir, "nope.jed")) }
      assert_equal "58P01", err.sqlstate
    end
  end

  def test_double_close_is_safe_and_use_after_close_guards
    db = Jed.memory
    db.close
    db.close # must not double-free
    assert db.closed?
    err = assert_raises(Jed::Error) { db.execute("SELECT 1") }
    assert_equal "XX000", err.sqlstate
  end

  # Drive a large result set so the wire framing (row/col loops, length prefixes) is exercised at
  # volume, and confirm coercion holds across every row.
  def test_many_rows_round_trip
    Jed.memory do |db|
      db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i64)")
      values = (1..500).map { |i| "(#{i}, #{i * i})" }.join(", ")
      assert_equal 500, db.execute("INSERT INTO t VALUES #{values}")[:rows_affected]
      res = db.query("SELECT id, v FROM t ORDER BY id")
      assert_equal 500, res.size
      assert_equal [1, 1], res.first.to_a
      assert_equal [500, 250_000], res.to_a.last.to_a
    end
  end
end
