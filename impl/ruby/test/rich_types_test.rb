# frozen_string_literal: true

require_relative "test_helper"
require "bigdecimal"
require "date"
require "time"

# Tests for the richer typed-value coercion (spec/design/ruby.md §3) — decimal⇄BigDecimal,
# date⇄Date, timestamp(tz)⇄Time, mirroring ActiveRecord's PostgreSQL mappings (including the
# `±infinity → ±Float::INFINITY` sentinel). These cover the GEM's marshalling seam; the underlying
# SQL types are the engine's, tested in the corpus + the Rust core.
class RichTypesTest < Minitest::Test
  # --- read: decimal → BigDecimal ---

  def test_decimal_reads_as_bigdecimal
    Jed.memory do |db|
      row = db.query("SELECT 19.99::decimal AS a, (-3.50)::decimal AS b, 0::decimal AS z").first
      assert_instance_of BigDecimal, row[:a]
      assert_equal BigDecimal("19.99"), row[:a]
      assert_equal BigDecimal("-3.5"), row[:b]
      assert_equal BigDecimal("0"), row[:z]
    end
  end

  def test_decimal_is_exact_not_float
    Jed.memory do |db|
      # the classic 0.1 + 0.2 — exact under decimal, never 0.30000000000000004
      row = db.query("SELECT 0.1::decimal + 0.2::decimal AS sum").first
      assert_equal BigDecimal("0.3"), row[:sum]
    end
  end

  # --- read: date → Date (with infinity + BC) ---

  def test_date_reads_as_date
    Jed.memory do |db|
      row = db.query("SELECT DATE '2020-01-02' AS d").first
      assert_instance_of Date, row[:d]
      assert_equal Date.new(2020, 1, 2), row[:d]
    end
  end

  def test_date_infinity_is_float_infinity
    Jed.memory do |db|
      row = db.query("SELECT 'infinity'::date AS p, '-infinity'::date AS n").first
      assert_equal Float::INFINITY, row[:p]
      assert_equal(-Float::INFINITY, row[:n])
    end
  end

  def test_bc_date_uses_astronomical_year
    Jed.memory do |db|
      # 44 BC → astronomical year -43 (1 - 44)
      row = db.query("SELECT DATE '0044-03-15 BC' AS d").first
      assert_instance_of Date, row[:d]
      assert_equal Date.new(-43, 3, 15), row[:d]
    end
  end

  # --- read: timestamp / timestamptz → Time (UTC) ---

  def test_timestamp_reads_as_utc_time
    Jed.memory do |db|
      row = db.query("SELECT TIMESTAMP '2020-01-02 03:04:05.123456' AS t").first
      assert_instance_of Time, row[:t]
      assert row[:t].utc?
      assert_equal Time.utc(2020, 1, 2, 3, 4, 5, 123_456), row[:t]
      assert_equal 123_456, row[:t].usec
    end
  end

  def test_timestamptz_reads_as_instant
    Jed.memory do |db|
      row = db.query("SELECT TIMESTAMPTZ '2020-01-02 03:04:05+00' AS t").first
      assert_instance_of Time, row[:t]
      assert_equal Time.utc(2020, 1, 2, 3, 4, 5), row[:t]
    end
  end

  def test_timestamp_infinity_is_float_infinity
    Jed.memory do |db|
      row = db.query("SELECT 'infinity'::timestamp AS p, '-infinity'::timestamptz AS n").first
      assert_equal Float::INFINITY, row[:p]
      assert_equal(-Float::INFINITY, row[:n])
    end
  end

  # --- bind: BigDecimal / Date / Time round-trip ---

  def test_bind_and_read_back_all_rich_types
    Jed.memory do |db|
      db.execute("CREATE TABLE t (id i32 PRIMARY KEY, amount decimal, d date, ts timestamptz)")
      db.execute("INSERT INTO t VALUES ($1, $2, $3, $4)",
        1, BigDecimal("19.99"), Date.new(2020, 1, 2), Time.utc(2021, 6, 15, 10, 30, 0))
      row = db.query("SELECT amount, d, ts FROM t WHERE id = $1", 1).first
      assert_equal BigDecimal("19.99"), row[:amount]
      assert_equal Date.new(2020, 1, 2), row[:d]
      assert_equal Time.utc(2021, 6, 15, 10, 30, 0), row[:ts]
    end
  end

  def test_bind_bigdecimal_in_predicate
    Jed.memory do |db|
      db.execute("CREATE TABLE t (id i32 PRIMARY KEY, amount decimal)")
      db.execute("INSERT INTO t VALUES (1, 19.99), (2, 5.00)")
      assert_equal [1], db.query("SELECT id FROM t WHERE amount = $1", BigDecimal("19.99")).map { |r| r[:id] }
    end
  end

  def test_bind_date_in_predicate
    Jed.memory do |db|
      db.execute("CREATE TABLE t (id i32 PRIMARY KEY, d date)")
      db.execute("INSERT INTO t VALUES (1, $1), (2, $2)", Date.new(2020, 1, 2), Date.new(2021, 1, 2))
      assert_equal [1], db.query("SELECT id FROM t WHERE d = $1", Date.new(2020, 1, 2)).map { |r| r[:id] }
    end
  end

  def test_bind_datetime_is_treated_as_timestamp
    Jed.memory do |db|
      db.execute("CREATE TABLE t (id i32 PRIMARY KEY, ts timestamptz)")
      # DateTime is a Date subclass but an instant — it must bind as a timestamp, not a date.
      db.execute("INSERT INTO t VALUES ($1, $2)", 1, DateTime.new(2021, 6, 15, 10, 30, 0))
      assert_equal Time.utc(2021, 6, 15, 10, 30, 0), db.query("SELECT ts FROM t WHERE id = 1").first[:ts]
    end
  end

  def test_bind_preserves_decimal_precision
    Jed.memory do |db|
      db.execute("CREATE TABLE t (id i32 PRIMARY KEY, amount decimal)")
      big = BigDecimal("123456789012345678901234567890.123456789")
      db.execute("INSERT INTO t VALUES ($1, $2)", 1, big)
      assert_equal big, db.query("SELECT amount FROM t WHERE id = 1").first[:amount]
    end
  end

  # --- bind guards ---

  def test_bind_non_finite_bigdecimal_raises
    Jed.memory do |db|
      db.execute("CREATE TABLE t (id i32 PRIMARY KEY, a decimal)")
      err = assert_raises(ArgumentError) { db.execute("INSERT INTO t VALUES (1, $1)", BigDecimal("Infinity")) }
      assert_match(/non-finite BigDecimal/, err.message)
    end
  end

  # A BigDecimal bound to an integer column is a clean engine type error, not a silent truncation.
  def test_bind_bigdecimal_to_int_column_is_type_error
    Jed.memory do |db|
      db.execute("CREATE TABLE t (id i32 PRIMARY KEY)")
      assert_raises(Jed::Error) { db.execute("INSERT INTO t VALUES ($1)", BigDecimal("1.5")) }
    end
  end
end
