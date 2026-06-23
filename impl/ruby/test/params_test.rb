# frozen_string_literal: true

require_relative "test_helper"

# Tests for `$N` bind-parameter marshalling through the wrap (spec/design/ruby.md §3a). The engine's
# parameter binding/typing is exercised in the Rust core's own tests (tests/params.rs) and is shared
# behavior; these cover the GEM's seam — encoding Ruby values into the param buffer, the variadic
# API, and that engine bind errors surface as Jed::Error.
class ParamsTest < Minitest::Test
  def setup
    @db = Jed.memory
    @db.execute("CREATE TABLE t (id i32 PRIMARY KEY, name text, score f64, active boolean)")
    @db.execute("INSERT INTO t VALUES (1, 'alice', 9.5, true), (2, 'bob', 7.25, false)")
  end

  def teardown
    @db.close
  end

  def test_int_param_point_lookup
    res = @db.query("SELECT name FROM t WHERE id = $1", 2)
    assert_equal "bob", res.first[:name]
  end

  def test_text_param
    res = @db.query("SELECT id FROM t WHERE name = $1", "alice")
    assert_equal 1, res.first[:id]
  end

  def test_float_and_boolean_params
    assert_equal 1, @db.query("SELECT id FROM t WHERE score = $1", 9.5).first[:id]
    assert_equal [1], @db.query("SELECT id FROM t WHERE active = $1", true).map { |r| r[:id] }
    assert_equal [2], @db.query("SELECT id FROM t WHERE active = $1", false).map { |r| r[:id] }
  end

  def test_multiple_params_and_reuse
    res = @db.query("SELECT id FROM t WHERE id = $1 AND name = $2", 1, "alice")
    assert_equal 1, res.first[:id]
    # $1 reused in two sites resolves to one value
    assert_equal [2], @db.query("SELECT id FROM t WHERE id = $1 OR id = $1", 2).map { |r| r[:id] }
  end

  def test_array_splat
    args = [1, "alice"]
    res = @db.query("SELECT id FROM t WHERE id = $1 AND name = $2", *args)
    assert_equal 1, res.first[:id]
  end

  def test_insert_update_delete_with_params
    Jed.memory do |db|
      db.execute("CREATE TABLE u (id i32 PRIMARY KEY, name text)")
      ins = db.execute("INSERT INTO u VALUES ($1, $2), ($3, $4)", 1, "a", 2, "b")
      assert_equal 2, ins[:rows_affected]
      upd = db.execute("UPDATE u SET name = $1 WHERE id = $2", "z", 1)
      assert_equal 1, upd[:rows_affected]
      assert_equal "z", db.query("SELECT name FROM u WHERE id = $1", 1).first[:name]
      del = db.execute("DELETE FROM u WHERE id = $1", 2)
      assert_equal 1, del[:rows_affected]
    end
  end

  def test_null_param_is_bound_as_sql_null
    @db.execute("INSERT INTO t VALUES ($1, $2, $3, $4)", 3, nil, nil, nil)
    row = @db.query("SELECT name, score FROM t WHERE id = $1", 3).first
    assert_nil row[:name]
    assert_nil row[:score]
    # ...and a NULL param matches via IS NULL, not = (3VL)
    assert_equal [3], @db.query("SELECT id FROM t WHERE name IS NULL").map { |r| r[:id] }
  end

  def test_i64_boundary_values_bind
    Jed.memory do |db|
      db.execute("CREATE TABLE b (id i64 PRIMARY KEY)")
      db.execute("INSERT INTO b VALUES ($1), ($2)", 2**63 - 1, -(2**63))
      assert_equal [-(2**63), 2**63 - 1], db.query("SELECT id FROM b ORDER BY id").map { |r| r[:id] }
    end
  end

  # Engine bind errors surface as Jed::Error with the right sqlstate (the binding is two-phase,
  # before any row is touched — api.md §5).

  def test_param_overflow_for_narrow_column_traps_22003
    Jed.memory do |db|
      db.execute("CREATE TABLE s (id i32 PRIMARY KEY, v i16)")
      db.execute("INSERT INTO s VALUES (1, 100)")
      err = assert_raises(Jed::Error) { db.query("SELECT id FROM s WHERE v = $1", 100_000) }
      assert_equal "22003", err.sqlstate
    end
  end

  def test_null_param_into_not_null_traps_23502
    Jed.memory do |db|
      db.execute("CREATE TABLE n (id i32 PRIMARY KEY, name text NOT NULL)")
      err = assert_raises(Jed::Error) { db.execute("INSERT INTO n VALUES ($1, $2)", 1, nil) }
      assert_equal "23502", err.sqlstate
    end
  end

  def test_indeterminate_param_type_is_42P18
    err = assert_raises(Jed::Error) { @db.query("SELECT $1", 5) }
    assert_equal "42P18", err.sqlstate
  end

  # Gem-side guards (raised before the FFI call, as ArgumentError — a programming error, not a
  # SQL one).

  def test_unsupported_param_type_raises_argument_error
    err = assert_raises(ArgumentError) { @db.query("SELECT id FROM t WHERE id = $1", [1, 2, 3]) }
    assert_match(/unsupported bind-parameter type Array/, err.message)
  end

  def test_integer_out_of_i64_range_raises_argument_error
    err = assert_raises(ArgumentError) { @db.query("SELECT id FROM t WHERE id = $1", 2**70) }
    assert_match(/out of range for a 64-bit integer/, err.message)
  end

  def test_param_round_trips_through_a_file
    Dir.mktmpdir do |dir|
      path = File.join(dir, "p.jed")
      Jed.create(path) do |db|
        db.execute("CREATE TABLE t (id i32 PRIMARY KEY, name text)")
        db.execute("INSERT INTO t VALUES ($1, $2)", 7, "persisted")
      end
      Jed.open(path) do |db|
        assert_equal "persisted", db.query("SELECT name FROM t WHERE id = $1", 7).first[:name]
      end
    end
  end

  # A statement with no params still works (the no-parameter fast path: null buffer, len 0).
  def test_no_params_still_works
    assert_equal 2, @db.query("SELECT count(*) AS n FROM t").first[:n]
  end
end
