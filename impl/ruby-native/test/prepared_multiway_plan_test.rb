# frozen_string_literal: true

require 'tmpdir'
require 'weakref'
require_relative '../lib/jed_native'

# Retained statements and suspended host cursors cannot be expressed by a fresh
# SQL transcript. The ordinary resolver remains the independent control here;
# shared P8 corpus entries continue to define join order, ownership and cost.
module NativePreparedMultiwayPlanTests
  module_function

  APIS = %i[query_prepared execute_prepared query execute].freeze
  JOIN = 'FROM pm_a a JOIN pm_b b ON b.id = a.k JOIN pm_c c ON c.id = a.id'

  def assert(condition, message) = (raise(message) unless condition)

  def setup(database)
    database.execute_script(<<~SQL)
      CREATE TABLE pm_a(id i32 PRIMARY KEY, k i32, v i32, flag boolean);
      CREATE TABLE pm_b(id i32 PRIMARY KEY, k i32, v i32, flag boolean);
      CREATE TABLE pm_c(id i32 PRIMARY KEY, k i32, v i32, flag boolean);
      CREATE TABLE pm_d(id i32 PRIMARY KEY, k i32, padding text, v i32, flag boolean);
      INSERT INTO pm_a VALUES (1,2,10,TRUE),(2,3,20,NULL),(3,9,30,FALSE);
      INSERT INTO pm_b VALUES (2,1,40,TRUE),(3,2,50,FALSE),(4,1,60,NULL);
      INSERT INTO pm_c VALUES (1,1,100,FALSE),(2,2,200,TRUE),(3,1,300,NULL);
      INSERT INTO pm_d VALUES (1,1,'first',1000,TRUE),(2,2,'second',2000,FALSE);
      CREATE INDEX pm_b_k ON pm_b(k);
    SQL
  end

  def session(database, disabled: false, **options)
    database.session(**options).tap { |current| current.engine.instance_variable_set(:@native_join_resolution_disabled, disabled) }
  end

  def cache(current) = current.engine.instance_variable_get(:@native_join_resolution_plans) || {}

  def cache_bounds(current)
    cache = cache(current)
    assert(cache.length <= JedNative::PreparedJoins::CACHE_ENTRIES, 'multiway statement cache exceeded its entry bound')
    cache.each_value do |entry|
      assert(entry.roots.length <= JedNative::PreparedJoins::ROOT_ENTRIES &&
        entry.key_units <= JedNative::PreparedJoins::KEY_UNITS && entry.key_bytes <= JedNative::PreparedJoins::KEY_BYTES,
        'multiway resolution metadata exceeded a retention bound')
    end
    assert(!current.engine.instance_variable_get(:@native_join_resolution_frame), 'multiway frame escaped its SELECT invocation')
  end

  def outcome(current, statement, sql, params, api, collect: false)
    lifetime, phase, values = current.lifetime_cost, :admission, []
    result = api.to_s.end_with?('_prepared') ? current.public_send(api, statement, *params) : current.public_send(api, sql, *params)
    phase = :pull
    if result.is_a?(JedNative::Rows) && !collect
      result.each { |row| values << row.to_a }
    else
      values = result.rows
    end
    [:rows, values, result.columns, result.column_types, result.cost, current.lifetime_cost - lifetime]
  rescue JedNative::Error => error
    [:error, phase, error.sqlstate, error.message, values, result&.cost || current.engine.cost, current.lifetime_cost - lifetime]
  ensure
    result&.close
  end

  def traced
    calls, plans = Hash.new(0), []
    tracker = TracePoint.new(:call, :return) do |event|
      if event.event == :call && event.defined_class == JedNative::Engine && event.method_id == :compile
        calls[:compile] += 1
      elsif event.event == :call && event.method_id == :nway_extension_inputs
        calls[:extension_requests] += 1 if event.defined_class == JedNative::PreparedJoins
        calls[:extension_resolutions] += 1 if event.defined_class == JedNative::NWayJoins
      elsif event.event == :return && event.defined_class == JedNative::PreparedJoins && event.method_id == :nway_join_plan
        plan = event.return_value
        next unless plan.is_a?(Hash)
        frame = event.self.instance_variable_get(:@native_join_resolution_frame)
        retained = frame&.extensions&.length || 0
        assert(retained <= JedNative::PreparedJoins::EXTENSION_ENTRIES, 'local extension memo exceeded its bound')
        calls[:maximum_extensions] = [calls[:maximum_extensions], retained].max
        access = ->(entry) { entry&.slice(:kind, :name, :scan_cost, :scan_rows, :output_rows, :pipeline_cost) }
        plans << plan.slice(:order, :ordered, :target, :cost, :rows, :logical, :output_rows).merge(
          accesses: plan[:accesses].map(&access),
          steps: plan[:steps].map { |step| step.slice(:algorithm, :kind, :cost, :rows, :logical, :work).merge(index: access.call(step.dig(:index, :access))) })
      end
    end
    value = tracker.enable { yield }
    [value, calls, plans.uniq]
  end

  def compare_current(enabled, disabled, statements, sql, params, api, trace: false, **options)
    if trace
      actual, counts, plans = traced { outcome(enabled, statements[0], sql, params, api, **options) }
      expected, ordinary_counts, ordinary_plans = traced { outcome(disabled, statements[1], sql, params, api, **options) }
      assert(plans == ordinary_plans, "multiway physical winner changed for #{params.inspect}: #{plans.inspect} != #{ordinary_plans.inspect}")
      @last_compile_counts = [counts[:compile], ordinary_counts[:compile]]
      @last_extension_counts = [counts[:extension_resolutions], counts[:extension_requests], counts[:maximum_extensions]]
      @last_physical_plans = plans.map do |plan|
        [plan[:order], plan[:accesses].map { |access| access&.values_at(:kind, :name) },
          plan[:steps].map { |step| [step[:algorithm], step[:index]&.values_at(:kind, :name)] }]
      end
    else
      actual = outcome(enabled, statements[0], sql, params, api, **options)
      expected = outcome(disabled, statements[1], sql, params, api, **options)
    end
    assert(actual == expected, "multiway reuse changed #{api} #{params.inspect} #{sql}: #{actual.inspect} != #{expected.inspect}")
    [enabled, disabled].each { |current| cache_bounds(current) }
    actual
  end

  def compare(database, sql, params, api:, warm_params: params, setting: nil, limit: 0, **options)
    enabled, disabled = session(database), session(database, disabled: true)
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    2.times { compare_current(enabled, disabled, statements, sql, warm_params, api, **options) }
    [enabled, disabled].each do |current|
      current.public_send("#{setting}=", setting == :lifetime_max_cost ? current.lifetime_cost + limit : limit) if setting
    end
    compare_current(enabled, disabled, statements, sql, params, api, **options)
  ensure
    enabled&.close
    disabled&.close
  end

  def reuse_and_scope_order(database)
    enabled = disabled = nil
    cases = [
      ["SELECT a.id, b.v, c.v #{JOIN} WHERE a.id >= $1 ORDER BY a.id", [[1], [2], [4], [nil], [1]]],
      ["SELECT a.id, b.id, c.id, d.id, d.v, d.padding #{JOIN} JOIN pm_d d ON d.id = c.id AND d.k = c.k WHERE a.id >= $1 ORDER BY a.id", [[1], [2], [4], [nil], [1]]],
      ['SELECT b.v, a.v, c.v FROM pm_b b JOIN pm_a a ON b.id = a.k CROSS JOIN pm_c c WHERE a.id = $1 AND c.id = 1 ORDER BY b.id', [[1], [2], [3], [nil], [1]]],
      ['SELECT a.id, b.v, c.v FROM pm_a a, pm_b b, pm_c c WHERE a.k = b.id AND c.id = a.id AND a.id = $1 ORDER BY a.id', [[1], [2], [3], [nil], [1]]],
      ["SELECT a.id, a.flag AND b.flag AND c.flag #{JOIN} WHERE a.id >= $1 ORDER BY a.id", [[1], [2], [nil], [1]]],
      ["SELECT a.id, b.id, c.id #{JOIN} WHERE a.id >= $1 ORDER BY a.id LIMIT $2 OFFSET $3", [[1, 1, 0], [1, 0, 0], [1, 2, 1], [2, 1, 0], [1, 1, 0]]]
    ]
    APIS.each do |api|
      cases.each do |sql, bindings|
        enabled, disabled = session(database), session(database, disabled: true)
        statements = [enabled, disabled].map { |current| current.prepare(sql) }
        2.times do
          bindings.each do |params|
            result = compare_current(enabled, disabled, statements, sql, params, api, trace: true)
            assert(result.first == :rows, 'multiway admission fixture failed before warming')
          end
        end
        assert(cache(enabled).values.any? { |entry| entry.roots.any? }, "#{api} did not retain multiway roots")
        assert(cache(disabled).empty?, 'disabled multiway resolver retained entries')
        reused, ordinary = @last_compile_counts
        assert(reused < ordinary, "#{api} did not reduce ordinary root compilation: #{reused}/#{ordinary}")
        enabled.engine.instance_variable_set(:@native_join_resolution_disabled, true)
        compare_current(enabled, disabled, statements, sql, bindings.first, api, trace: true)
        assert(@last_compile_counts.first == @last_compile_counts.last, 'bypass reused an already-warmed multiway entry')
        enabled.engine.instance_variable_set(:@native_join_resolution_disabled, false)
        compare_current(enabled, disabled, statements, sql, bindings.first, api)
        exposed = enabled.query_prepared(statements.first, *bindings.first)
        exposed.column_types.first.replace('text')
        exposed.columns.first.replace('caller_name')
        exposed.rows
        exposed.close
        compare_current(enabled, disabled, statements, sql, bindings.first, api)
        enabled.close
        disabled.close
      end
    end
  ensure
    enabled&.close
    disabled&.close
  end

  def budgets_and_errors(database)
    sql = "SELECT a.id, b.v / $1, c.v #{JOIN} WHERE a.id >= $2 ORDER BY a.id"
    %i[query_prepared execute_prepared].each do |api|
      [[2, 1], [0, 1], [0, 99]].each do |params|
        result = compare(database, sql, params, api: api, warm_params: [2, 1])
        total = result[-2]
        %i[max_cost lifetime_max_cost].each do |setting|
          (1..total + 1).each do |ceiling|
            compare(database, sql, params, api: api, warm_params: [2, 1], setting: setting, limit: ceiling)
          end
        end
      end
      [1, 63, 64, 127, 128, 255, 256, 511, 512, 1024, 65_536].each do |ceiling|
        [false, true].each do |collect|
          compare(database, sql, [2, 1], api: api, setting: :max_query_memory_bytes, limit: ceiling, collect: collect)
        end
      end
    end
    narrow = "SELECT a.v + $1, c.v #{JOIN} WHERE a.id = $2 ORDER BY a.id"
    APIS.each do |api|
      [[2**31, 1], [2**31, 99], [nil, 1], [1, 1]].each do |params|
        compare(database, narrow, params, api: api, warm_params: [1, 1])
      end
    end
  end

  def statistics_and_publication(database)
    enabled, disabled, writer = session(database), session(database, disabled: true), database.session
    sql = "SELECT a.id, b.v, c.v #{JOIN} WHERE b.k = $1 ORDER BY a.id"
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    actions = [nil, 'ANALYZE pm_b', 'UPDATE pm_b SET k = 1 WHERE id = 3',
      'CREATE INDEX pm_b_same_id ON pm_b(id)', 'DROP INDEX pm_b_same_id', 'DELETE FROM pm_b WHERE id = 4']
    actions.each do |action|
      writer.execute(action) if action
      [1, 2, 99, nil, 1].each do |value|
        compare_current(enabled, disabled, statements, sql, [value], :query_prepared, trace: true)
      end
    end
    enabled.begin
    enabled.execute('UPDATE pm_c SET v = 301 WHERE id = 1')
    assert(outcome(enabled, statements[0], sql, [1], :query_prepared)[1].first.last == 301, 'pending multiway join missed own write')
    enabled.execute('SAVEPOINT multiway_before_change')
    enabled.execute('UPDATE pm_c SET v = 401 WHERE id = 1')
    assert(outcome(enabled, statements[0], sql, [1], :query_prepared)[1].first.last == 401, 'multiway join retained old pending descriptor')
    enabled.execute('ROLLBACK TO multiway_before_change')
    assert(outcome(enabled, statements[0], sql, [1], :query_prepared)[1].first.last == 301, 'multiway rollback retained later pending value')
    enabled.rollback
    compare_current(enabled, disabled, statements, sql, [1], :query_prepared, trace: true)

    [enabled, disabled].each { |current| current.revoke(:select, 'pm_c') }
    denied = compare_current(enabled, disabled, statements, sql, [99], :query_prepared)
    assert(denied.first == :error && denied[2] == '42501', 'warm empty multiway join skipped permission admission')
    [enabled, disabled].each(&:reset_privileges)
    [enabled, disabled].each { |current| current.timezone = '+02:00' }
    compare_current(enabled, disabled, statements, sql, [1], :query_prepared, trace: true)

    writer.execute('DROP TABLE pm_c')
    writer.execute('CREATE TABLE pm_c(id i32 PRIMARY KEY, k i32, v text, flag boolean)')
    writer.execute("INSERT INTO pm_c VALUES(1,1,'replacement',TRUE)")
    changed = compare_current(enabled, disabled, statements, sql, [1], :query_prepared, trace: true)
    assert(changed[1] == [[1, 40, 'replacement']] && changed[3] == %w[i32 i32 text], 'multiway schema replacement retained old output ordinals or types')
  ensure
    enabled&.rollback if enabled&.engine&.transaction_open?
    enabled&.close
    disabled&.close
    writer&.close
  end

  def parameter_sensitive_winners(database)
    writer = database.session
    writer.execute_script(<<~SQL)
      CREATE TABLE pm_driver(id i32 PRIMARY KEY, k i32);
      CREATE TABLE pm_skew(id i32 PRIMARY KEY, bucket i32);
      CREATE TABLE pm_tail(id i32 PRIMARY KEY);
      CREATE INDEX pm_skew_bucket ON pm_skew(bucket);
    SQL
    writer.execute("INSERT INTO pm_driver VALUES #{(1..12).map { |id| "(#{id},#{id})" }.join(',')}")
    writer.execute("INSERT INTO pm_tail VALUES #{(1..12).map { |id| "(#{id})" }.join(',')}")
    writer.execute("INSERT INTO pm_skew VALUES #{(1..40).map { |id| "(#{id},#{id <= 35 ? 0 : id})" }.join(',')}")
    enabled, disabled = session(database), session(database, disabled: true)
    sql = 'SELECT a.id,b.id,c.id FROM pm_driver a JOIN pm_skew b ON b.id=a.k JOIN pm_tail c ON c.id=a.id WHERE b.bucket=$1 ORDER BY a.id LIMIT 10'
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    physical_winners = []
    [nil, 'ANALYZE pm_skew', 'UPDATE pm_skew SET bucket=0 WHERE id=36',
      'DROP INDEX pm_skew_bucket', 'CREATE INDEX pm_skew_bucket ON pm_skew(bucket)', 'DELETE FROM pm_skew WHERE id=2'].each do |action|
      writer.execute(action) if action
      [0, 36, 99, nil, 0].each do |value|
        compare_current(enabled, disabled, statements, sql, [value], :query_prepared, trace: true)
        physical_winners.concat(@last_physical_plans)
      end
    end
    assert(physical_winners.uniq.length > 1, "changing parameter/statistics fixture never changed a physical order or access winner: #{physical_winners.uniq.inspect}")
  ensure
    enabled&.close
    disabled&.close
    writer&.close
  end

  def suspended_snapshots(database)
    enabled, disabled, writer = session(database), session(database, disabled: true), database.session
    sql = "SELECT a.id + $1::i32, b.v, c.v #{JOIN} WHERE a.id >= $2 ORDER BY a.id"
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    2.times { compare_current(enabled, disabled, statements, sql, [0, 1], :query_prepared) }
    first = [enabled, disabled].each_with_index.map { |current, index| current.query_prepared(statements[index], 1, 1) }
    second = [enabled, disabled].each_with_index.map { |current, index| current.query_prepared(statements[index], 10, 2) }
    assert((first + second).all? { |cursor| cursor.cost.zero? }, 'multiway planning ran before first pull')
    writer.execute('UPDATE pm_c SET v = 299 WHERE id = 2')
    fresh = compare_current(enabled, disabled, statements, sql, [100, 1], :query_prepared)
    assert(fresh[1].last == [102, 50, 299], 'new multiway cursor retained old publication')
    assert(second.map(&:rows).uniq == [[[12, 50, 200]]] && first.map(&:rows).uniq == [[[2, 40, 100], [3, 50, 200]]],
      'suspended multiway cursors lost distinct bindings or snapshots')
    cursors = [enabled, disabled].each_with_index.map { |current, index| current.query_prepared(statements[index], 0, 1) }
    assert(cursors.map(&:next) == [[1, 40, 100], [1, 40, 100]], 'warm multiway cursor first row changed')
    costs = cursors.map(&:cost)
    cursors.each(&:close)
    assert(cursors.map(&:cost) == costs && database.instance_variable_get(:@pins).empty?, 'early multiway close did more work or retained pins')
  ensure
    (first.to_a + second.to_a + cursors.to_a).each(&:close)
    enabled&.close
    disabled&.close
    writer&.close
  end

  def fences_and_cache_bounds
    enabled = disabled = current = nil
    calls = []
    environment = JedNative::Environment.new.register_function(name: 'pm_probe', args: ['i32'], returns: 'i32') { |value| calls << value; value }
    JedNative.memory(environment: environment) do |database|
      setup(database)
      queries = [
        'SELECT a.id FROM pm_a a RIGHT JOIN pm_b b ON b.id = a.k JOIN pm_c c ON c.id = a.id',
        'SELECT a.id FROM pm_a a JOIN pm_b b ON b.id = a.k FULL JOIN pm_c c ON c.id = a.id',
        'SELECT id FROM pm_a JOIN pm_b USING(id) JOIN pm_c USING(id)',
        "WITH unused AS (SELECT 1) SELECT a.id #{JOIN}",
        "SELECT a.id #{JOIN} WHERE EXISTS(SELECT 1)",
        "SELECT count(*) #{JOIN}",
        "SELECT a.id, row_number() OVER (ORDER BY a.id) #{JOIN}",
        "SELECT DISTINCT a.id #{JOIN}",
        "SELECT a.id::text #{JOIN}",
        "SELECT a.id #{JOIN} WHERE 'row' LIKE 'r%'",
        "EXPLAIN (ANALYZE, VERBOSE) SELECT a.id #{JOIN} WHERE a.id = 1"
      ]
      queries.each do |sql|
        enabled, disabled = session(database), session(database, disabled: true)
        statements = [enabled, disabled].map { |item| item.prepare(sql) }
        2.times do
          result = compare_current(enabled, disabled, statements, sql, [], :query_prepared)
          assert(result.first == :rows, 'unsupported multiway fixture did not exercise a successful fallback')
        end
        assert(cache(enabled).empty?, "fenced multiway query entered resolution cache: #{sql}")
        enabled.close
        disabled.close
      end
      current = session(database)
      sql = "SELECT pm_probe(a.id) #{JOIN} ORDER BY a.id"
      prepared = current.prepare(sql)
      2.times { current.query_prepared(prepared).rows }
      calls.clear
      cursor = current.query_prepared(prepared)
      assert(cursor.cost.zero? && calls.empty?, 'multiway host fallback executed during cursor admission')
      assert(cursor.rows == [[1], [2]] && calls == [1, 2] && cache(current).empty?, 'multiway host fallback retained callback results')
      cursor.close
      (JedNative::PreparedJoins::CACHE_ENTRIES + 4).times do |index|
        sql = "SELECT a.id + #{index}, b.v, c.v #{JOIN} WHERE a.id = 1"
        statement = current.prepare(sql)
        2.times { current.query_prepared(statement).rows }
      end
      cache_bounds(current)
      assert(cache(current).compare_by_identity? && cache(current).length == JedNative::PreparedJoins::CACHE_ENTRIES,
        'multiway statement churn did not reach the bounded identity cache')
      current.close
    end
  ensure
    enabled&.close
    disabled&.close
    current&.close
  end

  def detached_cache
    JedNative.memory do |database|
      database.execute_script("CREATE TABLE pm_hold(id i32 PRIMARY KEY, label text); INSERT INTO pm_hold VALUES(1,'captured')")
      current = session(database)
      sql = 'SELECT a.id, $1 FROM pm_hold a JOIN pm_hold b ON b.id = a.id JOIN pm_hold c ON c.id = b.id WHERE a.label = $1'
      statement = current.prepare(sql)
      input = +'captured'
      2.times { current.query_prepared(statement, input).rows }
      cursor = current.query_prepared(statement, input)
      execution = cursor.instance_variable_get(:@engine)
      cursor.rows
      retained = cache(current)
      references = [current, current.engine, cursor, execution, input].map { |value| WeakRef.new(value) }
      current.close
      [retained, references]
    end
  end

  def input_boundary
    JedNative.memory do |database|
      database.execute('CREATE TABLE pm_limit(id i32 PRIMARY KEY)')
      current = session(database)
      current.query('SELECT 1').rows
      maximum = JedNative::PreparedJoins::MAX_INPUTS
      [maximum, maximum + 1].each do |width|
        inputs = (0...width).map { |index| "pm_limit p#{index}" }
        [inputs.join(' CROSS JOIN '), inputs.join(',')].each do |from|
          statement = current.prepare("SELECT p0.id FROM #{from}")
          # Admission only: subset-DP boundary behavior is already pinned by P8;
          # no need to run the full eight-input search in this lifetime test.
          frame = current.engine.send(:prepared_join_frame, statement.ast)
          assert(!frame.nil? == (width == maximum), 'multiway input-count admission boundary changed')
          assert(frame.entry.tables.length == width, 'multiway admission lost an input table') if frame
        end
      end
      cache_bounds(current)
      current.close
    end
  end

  def local_extension_memo
    JedNative.memory do |database|
      setup(database)
      current = session(database)
      sql = "SELECT a.id, b.v, c.v #{JOIN}"
      statement = current.prepare(sql)
      current.execute_prepared(statement)
      engine = current.engine
      frame = engine.send(:prepared_join_frame, statement.ast)
      engine.instance_variable_set(:@native_join_resolution_frame, frame)
      context = engine.send(:nway_join_plan, statement.ast[:from], statement.ast[:joins], nil, statement.ast)
      first = engine.send(:nway_driver, context, 0, context[:available][0].first)
      # A Pareto frontier may keep the same ordered prefix with different
      # estimates. Exercise that invariant without depending on the fixture's
      # incidental winning frontier or inventing a cached physical winner.
      second = first.merge(cost: first[:cost] + 7, rows: first[:rows] + 3, logical: first[:logical] + 5)
      frame.extension_context = frame.extensions = nil
      actual, calls, = traced do
        [first, second].map { |state| engine.send(:nway_extensions, context, state, 1) }
      end
      assert(calls[:extension_requests] == 2 && calls[:extension_resolutions] == 1 && frame.extensions.length == 1,
        'same-prefix local extension inputs were not reused within one search')
      engine.instance_variable_set(:@native_join_resolution_frame, nil)
      expected = [first, second].map { |state| engine.send(:nway_extensions, context, state, 1) }
      signature = lambda do |choices|
        choices.map do |state|
          [state.values_at(:order, :cost, :rows, :logical, :ordered),
            state[:steps].map { |step| step.slice(:algorithm, :kind, :work, :input_rows, :rows, :logical, :pairs, :prefix_cost, :cost) }]
        end
      end
      assert(actual.map(&signature) == expected.map(&signature), 'local extension reuse changed a physical candidate estimate')
      assert(signature.call(actual[0]) != signature.call(actual[1]), 'local extension reuse retained earlier state estimates')
      bound = actual.map { |choices| choices.find { |state| state[:steps].last[:algorithm] == :index } }
      assert(bound.all? && !bound[0][:steps].last[:index][:access].equal?(bound[1][:steps].last[:index][:access]),
        'local extension reuse aliased mutable index access candidates')
      engine.instance_variable_set(:@native_join_resolution_frame, frame)
      engine.send(:nway_extensions, context.dup, first, 1)
      assert(frame.extension_context != nil && !frame.extension_context.equal?(context) && frame.extensions.length == 1,
        'local extension inputs survived a different planning context')
      engine.instance_variable_set(:@native_join_resolution_frame, nil)
      cache_bounds(current)
      current.close
    end
  end

  def scope_snapshots
    JedNative.memory do |database|
      current = database.session
      engine = current.engine
      entry = JedNative::PreparedJoins::Entry.new(tables: [], context: [], roots: {}, columns: {}, scopes: {}, nodes: {}, key_units: 0, key_bytes: 0)
      frame = JedNative::PreparedJoins::Frame.new(entry: entry, depth: 0, plans: {})
      engine.instance_variable_set(:@native_join_resolution_frame, frame)
      columns = [{name: +'x', type: +'i32'}, {name: +'y', type: +'i16'}]
      x, y = {kind: :column, name: 'x'}, {kind: :column, name: 'y'}
      first = engine.compile(x, columns)
      units = entry.key_units
      second_size = engine.send(:prepared_join_key_size, [[:value, 1, 0], y], entry)
      engine.compile(y, columns.map(&:dup))
      assert(entry.scopes.length == 1 && entry.key_units == units + second_size[0], 'equivalent scopes were copied and charged once per root')
      assert(engine.compile(x.dup, columns.map(&:dup)).equal?(first), 'equivalent mutable metadata missed its scope token')
      columns.first[:type] = +'i64'
      widened = engine.compile(x, columns)
      assert(widened.type == 'i64' && !widened.equal?(first) && first.type == 'i32' && entry.scopes.length == 2,
        "mutating caller metadata changed a retained scope or reused its old type: #{[widened.type, widened.equal?(first), first.type, entry.scopes.length].inspect}")
      reordered = engine.compile(x, columns.reverse)
      assert(reordered.value == 1 && widened.value.zero? && entry.scopes.length == 3, 'scope interning lost ordered column offsets')
      assert(entry.nodes.length == 2, 'the same AST was copied again for each ordered scope')
      assert(entry.scopes.keys.all? { |scope| scope.frozen? && scope.all? { |column| column.frozen? && column.values.grep(String).all?(&:frozen?) } },
        'scope token metadata retained mutable caller state')
      engine.instance_variable_set(:@native_join_resolution_frame, nil)
      current.close
    end
  end

  def retention
    retained, references = Thread.new { detached_cache }.value
    assert(retained.values.any? { |entry| entry.roots.any? }, 'multiway lifetime probe did not retain metadata')
    3.times { GC.start }
    assert(references.none?(&:weakref_alive?), 'multiway resolution metadata retained a session, engine, cursor or host binding')
  end

  def run
    %i[memory file].each do |mode|
      runner = lambda do |database|
        setup(database)
        reuse_and_scope_order(database)
        budgets_and_errors(database)
        suspended_snapshots(database)
        parameter_sensitive_winners(database)
        statistics_and_publication(database)
      end
      if mode == :memory
        JedNative.memory(&runner)
      else
        Dir.mktmpdir('jed-ruby-prepared-multiway') do |directory|
          JedNative.create(File.join(directory, 'db'), page_size: 512, locking: :exclusive, &runner)
        end
      end
    end
    fences_and_cache_bounds
    input_boundary
    local_extension_memo
    scope_snapshots
    retention
    puts 'ruby-native prepared multiway joins: bounded reuse, physical winners, changing bindings/catalogs, suspended snapshots, fences and exact budget prefixes passed'
  end
end

NativePreparedMultiwayPlanTests.run
