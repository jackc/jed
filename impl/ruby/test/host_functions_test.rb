# frozen_string_literal: true

require_relative "test_helper"
require "bigdecimal"
require "date"

# Tests for gem host functions (spec/design/ruby.md §5b): Jed::ExtensionRegistry, single-row and
# batch kernels upcalling through Fiddle closures. These cover the GEM's seam — registration across
# the C ABI, argument/result marshalling, error mapping (including the batch failing-row rule), the
# lifetime and re-entrancy rules. The host-function semantics themselves (strictness, cost, the
# batched ABI's replay) are the engine's, tested per core (impl/rust/tests/host_functions.rs).
class HostFunctionsTest < Minitest::Test
  # `twice(i64) -> i64`, raising for `fail_at`, recording each call's row count in `calls`.
  def twice_registry(calls, batched:, fail_at: nil, volatility: :immutable, cost: 1)
    reg = Jed::ExtensionRegistry.new
    if batched
      reg.register_batch("twice", [:i64], :i64, volatility: volatility, cost: cost) do |xs, out|
        calls << xs.length
        xs.each do |x|
          raise ArgumentError, "twice refuses #{x}" if x == fail_at

          out << (x * 2)
        end
      end
    else
      reg.register("twice", [:i64], :i64, volatility: volatility, cost: cost) do |x|
        calls << 1
        raise ArgumentError, "twice refuses #{x}" if x == fail_at

        x * 2
      end
    end
    reg
  end

  # A table t(id i64 PRIMARY KEY, a i64) of n rows (g, g), with a NULL where g % 10 = 0.
  def with_table(reg, n)
    Jed.memory(extensions: reg) do |db|
      db.execute("CREATE TABLE t (id i64 PRIMARY KEY, a i64)")
      db.execute("INSERT INTO t SELECT g, CASE WHEN g % 10 = 0 THEN NULL ELSE g END " \
                 "FROM generate_series(1, #{n}) AS g")
      yield db
    end
  end

  # [rows, cost] of a query, or [sqlstate, message] of its error.
  def run_sql(db, sql)
    res = db.query(sql)
    [res.map(&:to_a), res.cost]
  rescue Jed::Error => e
    [e.sqlstate, e.message]
  end

  # --- single-row and batch kernels ---

  def test_single_row_kernel
    calls = []
    with_table(twice_registry(calls, batched: false), 30) do |db|
      rows = db.query("SELECT id, twice(a) FROM t ORDER BY id").map(&:to_a)
      assert_equal [1, 2], rows[0]
      assert_equal [10, nil], rows[9]
      assert_equal 27, calls.length # once per non-NULL row
    end
  end

  def test_batch_kernel_called_once_per_chunk
    # 2500 rows (250 NULL) through the buffered projection: three chunks of ≤1,024 rows, each one
    # upcall over its non-NULL rows (the Rust core's chunking, extensibility.md §4.2.1).
    calls = []
    with_table(twice_registry(calls, batched: true), 2500) do |db|
      rows = db.query("SELECT id, twice(a) FROM t").map(&:to_a)
      assert_equal 2500, rows.length
      rows.each { |id, v| (id % 10).zero? ? assert_nil(v) : assert_equal(id * 2, v) }
      assert_equal [922, 922, 406], calls
    end
  end

  def test_batch_matches_single_row_rows_and_cost
    [
      "SELECT id, twice(a) FROM t",
      "SELECT id, twice(a), twice(id) FROM t WHERE id > 100",
      "SELECT a, twice(a) FROM t ORDER BY a DESC LIMIT 7 OFFSET 3",
      "SELECT x.id, twice(y.a) FROM t AS x JOIN t AS y ON x.id = y.id + 1",
      "SELECT twice(count(*)) FROM t",
    ].each do |sql|
      batched = with_table(twice_registry([], batched: true), 1500) { |db| run_sql(db, sql) }
      single = with_table(twice_registry([], batched: false), 1500) { |db| run_sql(db, sql) }
      assert_equal single, batched, sql
    end
  end

  def test_volatile_batch_kernel_is_called_per_row
    calls = []
    with_table(twice_registry(calls, batched: true, volatility: :volatile), 30) do |db|
      db.query("SELECT twice(a) FROM t")
      assert_equal [1] * 27, calls
    end
  end

  def test_zero_argument_batch_kernel
    reg = Jed::ExtensionRegistry.new
    reg.register_batch("seven", [], :i32, volatility: :immutable) { |out| out << 7 }
    with_table(reg, 5) do |db|
      assert_equal [7] * 5, db.query("SELECT seven() FROM t").map { |r| r[0] }
    end
  end

  # --- marshalling ---

  def test_argument_and_result_types_round_trip
    reg = Jed::ExtensionRegistry.new
    reg.register("echo_i16", [:smallint], :i16) { |x| x }
    reg.register("echo_i32", [:int], :i32) { |x| x }
    reg.register("echo_f32", [:real], :f32) { |x| x }
    reg.register("echo_f64", ["double precision"], :f64) { |x| x }
    reg.register("echo_bool", [:bool], :boolean) { |x| x }
    reg.register("echo_dec", [:numeric], :decimal) { |x| x }
    reg.register("echo_text", [:text], :text) { |x| x }
    reg.register("echo_date", [:date], :date) { |x| x }
    reg.register("echo_ts", [:timestamp], :timestamp) { |x| x }
    reg.register("echo_tstz", [:timestamptz], :timestamptz) { |x| x }
    reg.register("classes", %i[i64 f64 boolean decimal text date timestamptz], :text) do |*args|
      args.map { |a| a.class.name }.join(",")
    end
    Jed.memory(extensions: reg) do |db|
      row = db.query(<<~SQL).first
        SELECT echo_i16(-7::i16) AS i16, echo_i32(2147483647::i32) AS i32, echo_f32(1.5::f32) AS f32,
               echo_f64('NaN'::f64) AS nan, echo_bool(true) AS b, echo_dec(-12.340) AS d,
               echo_text('héllo') AS t, echo_date(DATE '0044-03-15 BC') AS dt,
               echo_date('infinity'::date) AS dinf,
               echo_ts(TIMESTAMP '2020-06-01 12:34:56.789') AS ts,
               echo_tstz(TIMESTAMPTZ '2020-06-01 12:00:00+00') AS tstz,
               echo_ts('-infinity'::timestamp) AS tsinf,
               classes(1, 2.5::f64, false, 1.5, 'x', DATE '2020-01-01', TIMESTAMPTZ '2020-01-01 00:00:00+00') AS c
      SQL
      assert_equal(-7, row[:i16])
      assert_equal 2_147_483_647, row[:i32]
      assert_equal 1.5, row[:f32]
      assert row[:nan].nan?
      assert_equal true, row[:b]
      assert_equal BigDecimal("-12.340"), row[:d]
      assert_equal "héllo", row[:t]
      assert_equal Date.new(-43, 3, 15, Date::GREGORIAN), row[:dt]
      assert_equal Float::INFINITY, row[:dinf]
      assert_equal Time.utc(2020, 6, 1, 12, 34, 56, 789_000), row[:ts]
      assert_equal Time.utc(2020, 6, 1, 12), row[:tstz]
      assert_equal(-Float::INFINITY, row[:tsinf])
      assert_equal "Integer,Float,FalseClass,BigDecimal,String,Date,Time", row[:c]
    end
  end

  def test_integer_result_for_decimal_and_large_results
    reg = Jed::ExtensionRegistry.new
    reg.register("big", [:i64], :decimal) { |x| x * (2**70) }
    reg.register("pad", [:i64], :text) { |x| "x" * x }
    reg.register_batch("pads", [:i64], :text) { |xs, out| xs.each { |x| out << ("y" * x) } }
    Jed.memory(extensions: reg) do |db|
      assert_equal BigDecimal(3 * (2**70)), db.query("SELECT big(3)").first[0]
      # Results past the callback's scratch buffer arrive through the sink (ruby.md §5b).
      assert_equal 100_000, db.query("SELECT length(pad(100000))").first[0]
      db.execute("CREATE TABLE n (k i64 PRIMARY KEY)")
      db.execute("INSERT INTO n SELECT g FROM generate_series(1, 300) AS g")
      lens = db.query("SELECT k, pads(k * 50) FROM n").map { |r| [r[0] * 50, r[1].length] }
      lens.each { |want, got| assert_equal want, got }
    end
  end

  # --- NULL strictness ---

  def test_null_argument_skips_the_kernel_and_nil_result_is_null
    calls = 0
    reg = Jed::ExtensionRegistry.new
    reg.register("f", %i[i64 text], :text) { |x, _s| calls += 1; x.odd? ? nil : "even" }
    Jed.memory(extensions: reg) do |db|
      row = db.query("SELECT f(NULL::i64, 'a') AS a, f(1, NULL::text) AS b, f(1, 'a') AS c, f(2, 'a') AS d").first
      assert_equal [nil, nil, nil, "even"], row.to_a
      assert_equal 2, calls
    end
  end

  # --- errors ---

  def test_exception_becomes_38000_with_cause
    with_table(twice_registry([], batched: false, fail_at: 5), 10) do |db|
      err = assert_raises(Jed::Error) { db.query("SELECT twice(a) FROM t") }
      assert_equal "38000", err.sqlstate
      assert_equal "host function twice raised ArgumentError: twice refuses 5", err.raw_message
      assert_instance_of ArgumentError, err.cause
    end
  end

  def test_long_error_message_survives_the_boundary
    # The message outgrows the callback's scratch buffer, so the result goes through the sink.
    reg = Jed::ExtensionRegistry.new
    reg.register("loud", [:i64], :i64) { |x| raise "#{x}:" + ("é" * 5000) }
    reg.register_batch("louder", [:i64], :i64, volatility: :immutable) { |xs, _out| raise "#{xs.length}:" + ("é" * 5000) }
    Jed.memory(extensions: reg) do |db|
      err = assert_raises(Jed::Error) { db.query("SELECT loud(7)") }
      assert_equal "host function loud raised RuntimeError: 7:#{"é" * 5000}", err.raw_message
      err = assert_raises(Jed::Error) { db.query("SELECT louder(7)") }
      assert_equal "host function louder raised RuntimeError: 1:#{"é" * 5000}", err.raw_message
    end
  end

  def test_jed_error_keeps_its_sqlstate
    reg = Jed::ExtensionRegistry.new
    reg.register("safe_div", %i[i64 i64], :i64) do |a, b|
      raise Jed::Error.new("22012", "division by zero in safe_div") if b.zero?

      a / b
    end
    reg.register("odd_code", [:i64], :i64) { |_| raise Jed::Error.new("ZZ999", "custom") }
    Jed.memory(extensions: reg) do |db|
      assert_equal 3, db.query("SELECT safe_div(7, 2)").first[0]
      err = assert_raises(Jed::Error) { db.query("SELECT safe_div(1, 0)") }
      assert_equal ["22012", "division by zero in safe_div"], [err.sqlstate, err.raw_message]
      err = assert_raises(Jed::Error) { db.query("SELECT odd_code(1)") }
      assert_equal ["38000", "host function odd_code raised ZZ999: custom"], [err.sqlstate, err.raw_message]
    end
  end

  def test_batch_error_raises_at_the_scalar_row
    # The kernel fails at a = 57. Alone, that is the error. A division by zero at id = 40 is an
    # earlier row and wins — which needs the batch's prefix (rows 1..56 answered before the raise),
    # or the engine would report the kernel error at the chunk's first row. At id = 60 it is later
    # and the kernel's row wins. Batch and single-row kernels agree on every one.
    [
      ["SELECT twice(a) FROM t", "38000"],
      ["SELECT 1 / (id - 40), twice(a) FROM t", "22012"],
      ["SELECT twice(a), 1 / (id - 40) FROM t", "22012"],
      ["SELECT twice(a), 1 / (id - 60) FROM t", "38000"],
    ].each do |sql, code|
      batched = with_table(twice_registry([], batched: true, fail_at: 57), 200) { |db| run_sql(db, sql) }
      single = with_table(twice_registry([], batched: false, fail_at: 57), 200) { |db| run_sql(db, sql) }
      assert_equal code, batched[0], sql
      assert_equal single, batched, sql
    end
  end

  def test_wrong_typed_result_is_22000
    reg = Jed::ExtensionRegistry.new
    reg.register("liar", [:i64], :i64) { |_| "oops" }
    reg.register("symbolic", [:i64], :text) { |_| :sym }
    reg.register("floaty", [:i64], :i64) { |x| x.to_f }
    reg.register("intbool", [:i64], :boolean) { |x| x }
    reg.register("intfloat", [:i64], :f64) { |x| x }
    reg.register_batch("batch_liar", [:i64], :i64, volatility: :immutable) do |xs, out|
      xs.each { |x| out << (x == 3 ? "three" : x) }
    end
    with_table(reg, 5) do |db|
      %w[liar symbolic floaty intbool intfloat batch_liar].each do |f|
        err = assert_raises(Jed::Error) { db.query("SELECT id, #{f}(a) FROM t") }
        assert_equal "22000", err.sqlstate, f
      end
      # The batch's earlier rows were fine: a window that ends before row 3 succeeds.
      assert_equal [[1, 1], [2, 2]], db.query("SELECT id, batch_liar(a) FROM t WHERE id < 3").map(&:to_a)
    end
  end

  def test_out_of_range_integer_result_is_22003
    reg = Jed::ExtensionRegistry.new
    reg.register("wide", [:i32], :i32) { |x| x * 1_000_000 }
    reg.register("huge", [:i64], :i64) { |_| 2**64 }
    Jed.memory(extensions: reg) do |db|
      assert_equal 2_000_000, db.query("SELECT wide(2::i32)").first[0]
      assert_equal "22003", assert_raises(Jed::Error) { db.query("SELECT wide(5000::i32)") }.sqlstate
      assert_equal "22003", assert_raises(Jed::Error) { db.query("SELECT huge(1)") }.sqlstate
    end
  end

  def test_batch_shape_violations_are_22000
    reg = Jed::ExtensionRegistry.new
    reg.register_batch("short", [:i64], :i64, volatility: :immutable) { |xs, out| out.concat(xs.drop(1)) }
    reg.register_batch("long", [:i64], :i64, volatility: :immutable) { |xs, out| out.concat(xs + [0]) }
    with_table(reg, 20) do |db|
      %w[short long].each do |f|
        assert_equal "22000", assert_raises(Jed::Error) { db.query("SELECT #{f}(a) FROM t") }.sqlstate, f
      end
    end
  end

  def test_non_local_exit_is_contained
    reg = Jed::ExtensionRegistry.new
    reg.register("thrower", [:i64], :i64) { |_| throw :escape, 1 }
    Jed.memory(extensions: reg) do |db|
      err = nil
      caught = catch(:escape) do
        err = assert_raises(Jed::Error) { db.query("SELECT thrower(1)") }
        :not_thrown
      end
      assert_equal :not_thrown, caught
      assert_equal "38000", err.sqlstate
      assert_equal [[1]], db.query("SELECT 1").map(&:to_a) # the handle is still usable
    end
  end

  def test_interrupt_is_reraised_as_itself
    reg = Jed::ExtensionRegistry.new
    reg.register("ctrl_c", [:i64], :i64) { |_| raise Interrupt }
    Jed.memory(extensions: reg) do |db|
      assert_raises(Interrupt) { db.query("SELECT ctrl_c(1)") }
      assert_equal [[1]], db.query("SELECT 1").map(&:to_a)
    end
  end

  # --- cost ---

  def test_declared_cost_is_charged_per_call
    cost = lambda do |weight|
      with_table(twice_registry([], batched: true, cost: weight), 100) do |db|
        db.query("SELECT twice(a) FROM t").cost
      end
    end
    # 100 rows, each charging the declared weight once — a NULL row too, since the call charges before
    # its strict NULL check (extensibility.md §4.2).
    base = cost.call(0)
    assert_equal 100, cost.call(1) - base
    assert_equal 1000, cost.call(10) - base
  end

  # --- registration ---

  def test_registration_errors
    reg = Jed::ExtensionRegistry.new
    reg.register("f", [:i64], :i64) { |x| x }
    reg.register("f", [:text], :text) { |x| x } # an overload on another signature is fine
    {
      "42723" => -> { reg.register("F", [:bigint], :i64) { |x| x } },
      "22023" => -> { reg.register("g", [:i64], :i64, cost: -1) { |x| x } },
      "42704" => -> { reg.register("g", [:nonsense], :i64) { |x| x } },
      "0A000" => -> { reg.register("g", [:uuid], :i64) { |x| x } },
    }.each do |code, attempt|
      assert_equal code, assert_raises(Jed::Error) { attempt.call }.sqlstate
    end
    assert_raises(ArgumentError) { reg.register("g", [:i64], :i64) }
    assert_raises(ArgumentError) { reg.register("g", [:i64], :i64, volatility: :sometimes) { |x| x } }
    assert_raises(ArgumentError) { reg.register("g", [42], :i64) { |x| x } }
  end

  def test_a_handle_freezes_the_functions_registered_at_open
    reg = Jed::ExtensionRegistry.new
    reg.register("one", [], :i64) { 1 }
    Jed.memory(extensions: reg) do |db|
      reg.register("two", [], :i64) { 2 }
      assert_equal 1, db.query("SELECT one()").first[0]
      assert_equal "42883", assert_raises(Jed::Error) { db.query("SELECT two()") }.sqlstate
      Jed.memory(extensions: reg) { |db2| assert_equal 2, db2.query("SELECT two()").first[0] }
    end
  end

  def test_file_backed_handles_take_extensions
    reg = Jed::ExtensionRegistry.new
    reg.register("inc", [:i64], :i64) { |x| x + 1 }
    Dir.mktmpdir do |dir|
      path = File.join(dir, "h.jed")
      Jed.create(path, extensions: reg) { |db| assert_equal 2, db.query("SELECT inc(1)").first[0] }
      Jed.open(path, extensions: reg) { |db| assert_equal 3, db.query("SELECT inc(2)").first[0] }
      Jed.open(path, read_only: true, extensions: reg) { |db| assert_equal 4, db.query("SELECT inc(3)").first[0] }
      Jed.open(path) { |db| assert_equal "42883", assert_raises(Jed::Error) { db.query("SELECT inc(1)") }.sqlstate }
    end
  end

  # --- lifetime and re-entrancy ---

  def test_closures_outlive_the_registry_object
    db = Jed.memory(extensions: Jed::ExtensionRegistry.new.register("seven", [], :i64) { 7 })
    3.times { GC.start(full_mark: true, immediate_sweep: true) }
    assert_equal 7, db.query("SELECT seven()").first[0]
  ensure
    db&.close
  end

  def test_reentrant_use_of_the_calling_handle_is_refused
    db = nil
    other = Jed.memory
    reg = Jed::ExtensionRegistry.new
    reg.register("same", [:i64], :i64) { |x| db.query("SELECT 1").first[0] + x }
    reg.register("other", [:i64], :i64) { |x| other.query("SELECT 10").first[0] + x }
    reg.register("closer", [:i64], :i64) { |x| db.close || x }
    db = Jed.memory(extensions: reg)
    err = assert_raises(Jed::Error) { db.query("SELECT same(1)") }
    assert_equal "55006", err.sqlstate
    err = assert_raises(Jed::Error) { db.query("SELECT closer(1)") }
    assert_equal "55006", err.sqlstate
    refute db.closed?
    assert_equal 11, db.query("SELECT other(1)").first[0] # another handle is fine
  ensure
    db&.close
    other&.close
  end

  def test_concurrent_threads_share_a_handle_safely
    reg = Jed::ExtensionRegistry.new
    reg.register_batch("twice", [:i64], :i64, volatility: :immutable) { |xs, out| xs.each { |x| out << (x * 2) } }
    with_table(reg, 2000) do |db|
      sums = Array.new(4) do
        Thread.new { Array.new(5) { db.query("SELECT sum(twice(a)) FROM t").first[0] } }
      end.flat_map(&:value)
      want = 2 * (1..2000).reject { |g| (g % 10).zero? }.sum
      assert_equal [want] * 20, sums
    end
  end
end
