# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

# The SQL corpus owns successful query semantics. These host checks use small
# pages to pin multi-leaf charge blocks, pull/fault ordering and cost ceilings
# across the eager and lazy APIs.
module NativeScanCostTests
  module_function

  def assert(condition, message) = (raise(message) unless condition)

  def with_database(mode)
    if mode == :memory
      JedNative.memory(page_size: 256) { |database| yield database }
    else
      Dir.mktmpdir('jed-native-scan-cost') do |directory|
        path = File.join(directory, 'db')
        JedNative.create(path, page_size: 256, locking: :exclusive) { |database| yield database }
      end
    end
  end

  def populate(database)
    session = database.session
    session.execute('CREATE TABLE t(id i32 PRIMARY KEY, v i32)')
    session.execute('INSERT INTO t SELECT g::i32, g::i32 FROM generate_series(1,120) g')
    session
  end

  def run_api(session, api, sql)
    result = if api.to_s.end_with?('_prepared')
               session.public_send(api, session.prepare(sql))
             else
               session.public_send(api, sql)
             end
    rows = result.is_a?(JedNative::Rows) ? result.each.to_a : result.rows
    [rows, result.cost]
  ensure
    result&.close if result.is_a?(JedNative::Rows)
  end

  def ordered_page_blocks(database)
    session = populate(database)
    table = session.engine.tables.fetch('t')
    engine = session.engine
    full = engine.primary_page_count(table)
    assert(full > table[:native_primary_metadata][:height], 'fixture needs leaves beyond the first descent')
    lower = engine.primary_prefix_key(table, [40])
    upper = engine.primary_prefix_key(table, [80])
    bounded = engine.primary_page_count(table, lower)
    before = engine.primary_page_count(table, nil, upper)
    cases = [
      ['SELECT id FROM t ORDER BY id LIMIT 2', [[1], [2]], full + 4],
      ['SELECT id FROM t ORDER BY id DESC LIMIT 2', [[120], [119]], full + 4],
      ['SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 31', [[32], [33]], full + 35],
      ['SELECT id FROM t WHERE id >= 40 ORDER BY id LIMIT 2', [[40], [41]], bounded + 6],
      ['SELECT id FROM t WHERE id < 80 ORDER BY id DESC LIMIT 2 OFFSET 3', [[76], [75]], before + 12],
      ['SELECT id FROM t ORDER BY id LIMIT 0', [], full]
    ]
    %i[execute query execute_prepared query_prepared].each do |api|
      cases.each do |sql, rows, cost|
        assert(run_api(session, api, sql) == [rows, cost], "#{api} changed logical page block: #{sql}")
      end
    end
  ensure
    session&.close
  end

  def payload_blocks(database)
    session = database.session
    session.execute('CREATE TABLE payload(id i32 PRIMARY KEY, body text)')
    session.execute("INSERT INTO payload VALUES (1, 'small'), (2, '#{'x' * 600}')")
    # Row 2 needs three decompression slabs even when LIMIT emits only row 1,
    # emits no rows, or starts at the other end of the primary-key walk.
    ['ORDER BY id LIMIT 1', 'ORDER BY id DESC LIMIT 1', 'ORDER BY id LIMIT 0'].each do |tail|
      %i[execute query execute_prepared query_prepared].each do |api|
        _, untouched = run_api(session, api, "SELECT id FROM payload #{tail}")
        _, touched = run_api(session, api, "SELECT body FROM payload #{tail}")
        assert(touched - untouched == 3, "#{api} LIMIT skipped the bound's payload block: #{tail}")
      end
    end
  ensure
    session&.close
  end

  def structural_guard_before_row_access(database)
    session = database.session
    engine = session.engine
    table = engine.tables.fetch('t')
    pages = engine.primary_page_count(table)
    engine.instance_variable_set(:@native_scan_columns, { 't' => [0] })
    trace = []
    engine.define_singleton_method(:storage_row) do |*arguments|
      trace << :row
      super(*arguments)
    end
    (1..pages).each do |ceiling|
      engine.instance_variable_set(:@cost, 0)
      engine.max_cost = ceiling
      begin
        engine.storage_stream_rows(table).first
        raise 'expected page-block ceiling failure'
      rescue JedNative::Error => error
        assert(error.sqlstate == '54P01' && engine.cost == pages, 'page block was split across row pulls')
        assert(trace.empty?, 'page-block failure resolved a row before guarding')
      end
    end
    engine.max_cost = 0
    engine.instance_variable_set(:@cost, 0)
    views = 0
    probe = TracePoint.new(:call) do |event|
      views += 1 if event.defined_class == JedNative::PagedRows && event.method_id == :row_at
    end
    probe.enable do
      input = engine.storage_stream_rows(table)
      assert(engine.cost == pages && views.zero?, 'pricing fixed-width rows pulled data eagerly')
      assert(input.first[0] == 1, 'first row changed')
    end
    assert(views <= 1 && trace == [:row], 'short scan constructed later row views')
    engine.define_singleton_method(:storage_row) { |*| raise JedNative::Error.new('XX001', 'injected row fault') }
    engine.instance_variable_set(:@cost, 0)
    engine.max_cost = pages + 1
    begin
      engine.storage_stream_rows(table).first
      raise 'expected injected row fault'
    rescue JedNative::Error => error
      assert(error.sqlstate == 'XX001' && engine.cost == pages, 'row fault moved ahead of the page block')
    end
  ensure
    session&.close
  end

  def interval_blocks(database)
    session = database.session
    session.execute('CREATE TABLE tuples(a i32, b i32, v i32, PRIMARY KEY(a,b))')
    values = (1..3).flat_map { |a| (1..70).map { |b| "(#{a},#{b},#{a})" } }.join(',')
    session.execute("INSERT INTO tuples VALUES #{values}")
    session.execute('CREATE INDEX tuples_v_b ON tuples(v,b)')
    engine = session.engine
    table = engine.tables.fetch('tuples')
    scope = engine.table_columns(table, 'tuples')
    engine.instance_variable_set(:@native_stream_scan, true)
    engine.instance_variable_set(:@native_scan_columns, { 'tuples' => [0, 1, 2] })
    [['a', :primary_interval], ['v', :secondary_interval]].each do |column, kind|
      statement = JedNative::Statement.new("SELECT a,b FROM tuples WHERE #{column} IN (1,3) AND b >= 1").ast
      predicate = engine.compile_filter(statement[:where], scope)
      access = engine.scan_access_candidates(table, predicate).find { |candidate| candidate[:kind] == kind && candidate[:tuple_members] }
      assert(access, "missing #{kind} fixture")
      intervals = engine.send(:tuple_scan_intervals, table, access)
      pages = intervals.map { |lower, upper| engine.storage_tree_page_count(table, access[:name], lower: lower, upper: upper) }
      assert(pages.length == 2 && pages.all? { |count| count > 1 }, 'intervals must each span multiple pages')
      lookup = access[:name] ? table[:native_primary_metadata][:height] : 0
      { 0 => 0, 1 => pages.first + lookup, 71 => pages.sum + lookup * 71 }.each do |count, expected|
        engine.instance_variable_set(:@cost, 0)
        input = engine.send(:execute_interval_access, table, predicate, access)
        assert(engine.cost.zero?, 'unstarted interval was charged')
        rows = input.take(count)
        assert(rows.length == count && engine.cost == expected,
          "#{kind} consumed #{count} rows at cost #{engine.cost}, expected #{expected}")
      end
    end
  ensure
    session&.close
  end

  def cost_outcome(database, api, sql, setting, ceiling)
    session = database.session
    prior = session.lifetime_cost
    session.public_send("#{setting}=", setting == :lifetime_max_cost ? prior + ceiling : ceiling)
    result = nil
    begin
      result = api.to_s.end_with?('_prepared') ? session.public_send(api, session.prepare(sql)) : session.public_send(api, sql)
      rows = result.is_a?(JedNative::Rows) ? result.each.to_a : result.rows
      [:ok, result.cost, session.lifetime_cost - prior, rows]
    rescue JedNative::Error => error
      [error.sqlstate, result&.cost || session.engine.cost, session.lifetime_cost - prior]
    ensure
      result&.close if result.is_a?(JedNative::Rows)
      session.close
    end
  end

  def correlated_budget_prefixes(database)
    session = database.session
    session.execute_script('CREATE TABLE s(id i32 PRIMARY KEY, v i32); INSERT INTO s VALUES (1,10),(2,20); ' \
                           'CREATE TABLE o(id i32 PRIMARY KEY, v i32); INSERT INTO o VALUES (1,10),(2,20)')
    sql = 'SELECT (SELECT count(*) FROM s WHERE s.v = o.v) FROM o'
    assert(run_api(session, :execute, sql) == [[[1], [1]], 21], 'correlated baseline changed')
    session.close
    %i[max_cost lifetime_max_cost].each do |setting|
      (1..22).each do |ceiling|
        expected = cost_outcome(database, :execute, sql, setting, ceiling)
        %i[query execute_prepared query_prepared].each do |api|
          actual = cost_outcome(database, api, sql, setting, ceiling)
          assert(actual == expected, "#{api} changed #{setting}=#{ceiling}: #{actual.inspect} != #{expected.inspect}")
        end
        assert(expected[0] == :ok, 'exact final unguarded output must complete') if ceiling == 21
      end
    end
  ensure
    session&.close
  end

  def run
    %i[memory file].each do |mode|
      with_database(mode) do |database|
        ordered_page_blocks(database)
        payload_blocks(database)
        structural_guard_before_row_access(database)
        interval_blocks(database)
        correlated_budget_prefixes(database)
      end
    end
    puts 'ruby-native scan cost: full bound blocks, lazy row access, payload slabs and correlated budget prefixes passed'
  end
end

NativeScanCostTests.run
