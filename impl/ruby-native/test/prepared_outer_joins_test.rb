# frozen_string_literal: true

require 'tmpdir'
require 'weakref'
require_relative '../lib/jed_native'

# Shared join transcripts define NULL extension and predicate ownership. These
# probes retain prepared statements while bindings, catalogs and cursors change.
module NativePreparedOuterJoinTests
  APIS = %i[query_prepared execute_prepared query execute].freeze
  FROM = 'FROM oj_a a LEFT JOIN oj_b b ON a.k=b.k LEFT JOIN oj_c c ON c.id=b.id'
  module_function

  def assert(value, message) = (raise message unless value)

  def setup(database)
    database.execute_script(<<~SQL)
      CREATE TABLE oj_a(id i32 PRIMARY KEY,k i32,narrow i16,flag boolean);
      CREATE TABLE oj_b(id i32 PRIMARY KEY,k i32,v i32,label text,flag boolean);
      CREATE TABLE oj_c(id i32 PRIMARY KEY,v i32);
      CREATE TABLE oj_d(id i32 PRIMARY KEY,padding text,v i32);
      CREATE TABLE oj_empty(id i32 PRIMARY KEY,label text);
      INSERT INTO oj_a VALUES(1,10,1,TRUE),(2,20,2,NULL),(3,30,3,FALSE),(4,NULL,4,TRUE);
      INSERT INTO oj_b VALUES(10,10,100,'ten',TRUE),(20,20,200,'twenty',FALSE),(21,20,210,'twentyish',NULL),(30,NULL,300,'null',TRUE);
      INSERT INTO oj_c VALUES(10,1000),(20,2000),(30,3000);
      INSERT INTO oj_d VALUES(10,'d10',10),(20,'d20',20);
      CREATE INDEX oj_b_k ON oj_b(k);
    SQL
  end

  def session(database, disabled: false)
    database.session.tap { |current| current.engine.instance_variable_set(:@native_join_resolution_disabled, disabled) }
  end

  def cache(current) = current.engine.instance_variable_get(:@native_join_resolution_plans) || {}

  def check_bounds(current, no_physical: true)
    entries = cache(current)
    assert(entries.length <= JedNative::PreparedJoins::CACHE_ENTRIES, 'outer statement cache exceeded its bound')
    entries.each_value do |entry|
      assert(entry.roots.length <= JedNative::PreparedJoins::ROOT_ENTRIES &&
        entry.key_units <= JedNative::PreparedJoins::KEY_UNITS &&
        entry.key_bytes <= JedNative::PreparedJoins::KEY_BYTES, 'outer resolution metadata exceeded its bounds')
      assert(entry.columns.length <= JedNative::PreparedJoins::MAX_INPUTS &&
        entry.sources.length <= JedNative::PreparedJoins::MAX_INPUTS &&
        entry.source_units <= JedNative::PreparedJoins::KEY_UNITS &&
        entry.source_bytes <= JedNative::PreparedJoins::KEY_BYTES, 'outer source metadata exceeded its bounds')
      assert(entry.sources.values.all? { |columns| columns.frozen? && columns.all?(&:frozen?) },
        'outer source metadata retained mutable descriptor arrays')
      assert(entry.physical.nil?, 'fresh multiway LEFT entry retained a physical plan') if no_physical
    end
    assert(current.engine.instance_variable_get(:@native_join_resolution_frame).nil?, 'outer frame escaped its SELECT')
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

  def plan_image(plan)
    return unless plan.is_a?(Hash)
    access = ->(item) { item&.slice(:kind, :name, :scan_cost, :scan_rows, :output_rows, :logical_rows, :pipeline_cost) }
    plan.slice(:order, :mask, :ordered, :target, :cost, :rows, :logical, :output_rows, :join_cost).merge(
      accesses: plan[:accesses].map(&access),
      steps: plan[:steps].map do |step|
        step.slice(:algorithm, :kind, :cost, :rows, :logical, :pairs, :prefix_cost, :work)
          .merge(index: access.call(step.dig(:index, :access)))
      end)
  end

  def traced
    counts, plans = Hash.new(0), []
    trace = TracePoint.new(:call, :return) do |event|
      if event.event == :call
        counts[:compile] += 1 if event.defined_class == JedNative::Engine && event.method_id == :compile
        counts[:planner] += 1 if event.defined_class == JedNative::NWayJoins && event.method_id == :nway_join_plan
        counts[:source] += 1 if event.defined_class == JedNative::QueryColumns && event.method_id == :query_source_columns
        if event.method_id == :nway_extension_inputs
          counts[:extension_requests] += 1 if event.defined_class == JedNative::PreparedJoins
          counts[:extension_resolutions] += 1 if event.defined_class == JedNative::NWayJoins
        end
      elsif event.defined_class == JedNative::PreparedJoins && event.method_id == :nway_join_plan
        image = plan_image(event.return_value)
        plans << image if image
      end
    end
    value = trace.enable { yield }
    [value, counts, plans.uniq]
  end

  def compare_current(enabled, disabled, statements, sql, params, api, trace: false, no_physical: true, **options)
    if trace
      actual, counts, plans = traced { outcome(enabled, statements[0], sql, params, api, **options) }
      expected, original_counts, original_plans = traced { outcome(disabled, statements[1], sql, params, api, **options) }
      assert(plans == original_plans, "outer physical winner changed #{params.inspect}")
      @last_counts = [counts, original_counts]
    else
      actual = outcome(enabled, statements[0], sql, params, api, **options)
      expected = outcome(disabled, statements[1], sql, params, api, **options)
    end
    assert(actual == expected, "outer reuse changed #{api} #{sql} #{params.inspect}: #{actual.inspect} != #{expected.inspect}")
    check_bounds(enabled, no_physical: no_physical)
    check_bounds(disabled, no_physical: no_physical)
    actual
  end

  def compare(database, sql, params, api:, warm: params, setting: nil, limit: 0, **options)
    enabled, disabled = session(database), session(database, disabled: true)
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    2.times { compare_current(enabled, disabled, statements, sql, warm, api, **options) }
    [enabled, disabled].each do |current|
      current.public_send("#{setting}=", setting == :lifetime_max_cost ? current.lifetime_cost + limit : limit) if setting
    end
    compare_current(enabled, disabled, statements, sql, params, api, **options)
  ensure
    enabled&.close
    disabled&.close
  end

  def reuse_and_null_extension(database)
    enabled = disabled = nil
    cases = [
      ["SELECT a.id,b.id,b.label,c.v #{FROM} WHERE a.id>=$1 ORDER BY a.id,b.id", [[1], [2], [3], [99], [nil], [1]]],
      ['SELECT a.id,b.id,c.v FROM oj_a a JOIN oj_b b ON a.k=b.k LEFT JOIN oj_c c ON c.id=b.id WHERE a.id=$1 ORDER BY b.id',
        [[1], [2], [3], [nil], [1]]],
      ['SELECT a.id,b.id,c.v FROM oj_a a LEFT JOIN oj_b b ON a.k=b.k JOIN oj_c c ON c.id=b.id WHERE a.id=$1 ORDER BY b.id',
        [[1], [2], [3], [nil], [1]]],
      ['SELECT a.id,b.id,c.v,d.padding,d.v FROM oj_a a LEFT JOIN oj_b b ON a.k=b.k CROSS JOIN oj_c c JOIN oj_d d ON d.id=c.id WHERE a.id=$1 AND c.id=10 ORDER BY a.id,b.id,c.id,d.id',
        [[1], [2], [3], [nil], [1]]],
      ['SELECT a.id,e.label,c.v FROM oj_a a LEFT JOIN oj_empty e ON e.id=a.id LEFT JOIN oj_c c ON c.id=e.id WHERE a.id=$1',
        [[1], [4], [99], [nil], [1]]],
      ["SELECT * #{FROM} WHERE a.id=$1 ORDER BY b.id", [[1], [2], [3], [nil], [1]]]
    ]
    APIS.each do |api|
      cases.each do |sql, bindings|
        enabled, disabled = session(database), session(database, disabled: true)
        statements = [enabled, disabled].map { |current| current.prepare(sql) }
        2.times do
          bindings.each do |params|
            actual = compare_current(enabled, disabled, statements, sql, params, api, trace: true)
            assert(actual.first == :rows, 'outer admission fixture failed')
          end
        end
        entries = cache(enabled).values
        assert(entries.any? { |entry| entry.roots.any? } && entries.any? { |entry| entry.sources.any? },
          "#{api} did not admit multiway LEFT resolution/source metadata")
        assert(cache(disabled).empty?, 'disabled resolution retained an outer entry')
        counts, original = @last_counts
        assert(counts[:compile] < original[:compile], 'outer cache did not replace original root compilation')
        assert(counts[:source] < original[:source], 'outer metadata cache did not replace descriptor construction')
        assert(counts[:planner].positive? && counts[:planner] == original[:planner],
          'warmed multiway LEFT skipped or repeated physical winner selection')
        enabled.engine.instance_variable_set(:@native_join_resolution_disabled, true)
        compare_current(enabled, disabled, statements, sql, bindings.first, api, trace: true)
        assert(@last_counts[0] == @last_counts[1], 'disable switch consulted an already-warmed outer entry')
        enabled.close
        disabled.close
      end
      whole = compare(database, cases[0][0], [1], api: api)
      assert(whole[1] == [[1, 10, 'ten', 1000], [2, 20, 'twenty', 2000], [2, 21, 'twentyish', nil],
        [3, nil, nil, nil], [4, nil, nil, nil]], 'outer chain lost unmatched rows, NULL keys or duplicate matches')
      on = "SELECT a.id,b.id,c.v FROM oj_a a LEFT JOIN oj_b b ON a.k=b.k AND b.label=$1 LEFT JOIN oj_c c ON c.id=b.id ORDER BY a.id,b.id"
      where = "SELECT a.id,b.id,c.v #{FROM} WHERE b.label=$1 ORDER BY a.id,b.id"
      [[on, [[1, nil, nil], [2, 20, 2000], [3, nil, nil], [4, nil, nil]]],
       [where, [[2, 20, 2000]]]].each do |sql, expected|
        result = compare(database, sql, ['twenty'], api: api, warm: ['missing'])
        assert(result[1] == expected, 'outer cache moved a predicate across ON/WHERE NULL extension')
        compare(database, sql, [nil], api: api, warm: ['twenty'])
      end
      anti = compare(database, "SELECT a.id #{FROM} WHERE b.id IS NULL ORDER BY a.id", [], api: api)
      assert(anti[1] == [[3], [4]], 'outer anti-join changed')
    end
  ensure
    enabled&.close
    disabled&.close
  end

  def budgets(database)
    sql = "SELECT a.id,b.v/$1,c.v #{FROM} WHERE a.id=$2 ORDER BY b.id"
    APIS.each do |api|
      [[2, 2], [0, 2], [0, 3]].each do |params|
        actual = compare(database, sql, params, api: api, warm: [2, 2])
        assert(actual[0] == :error && actual[2] == '22012', 'matched division did not fail') if params == [0, 2]
        assert(actual[0, 2] == [:rows, [[3, nil, nil]]], 'NULL-extended division evaluated a missing row') if params == [0, 3]
        total = actual[-2]
        ceilings = api.to_s.end_with?('_prepared') ? (1..total + 1).to_a : [1, total, total + 1].uniq
        %i[max_cost lifetime_max_cost].each do |setting|
          ceilings.each { |limit| compare(database, sql, params, api: api, warm: [2, 2], setting: setting, limit: limit) }
        end
      end
      narrow = "SELECT a.narrow+$1,c.v #{FROM} WHERE a.id=$2 ORDER BY b.id"
      [[40_000, 1], [40_000, 99], [nil, 1], [1, 1]].each do |params|
        compare(database, narrow, params, api: api, warm: [1, 1])
      end
    end
    [1, 64, 192, 512, 4096].each do |limit|
      [false, true].each do |collect|
        compare(database, sql, [2, 2], api: :query_prepared,
          setting: :max_query_memory_bytes, limit: limit, collect: collect)
      end
    end
  end

  def invalidation_and_snapshots(database)
    enabled, disabled, writer = session(database), session(database, disabled: true), database.session
    sql = "SELECT a.id,b.v,c.v #{FROM} WHERE a.id=$1 ORDER BY b.id"
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    2.times { compare_current(enabled, disabled, statements, sql, [1], :query_prepared) }
    old = [enabled, disabled].each_with_index.map { |current, i| current.query_prepared(statements[i], 1) }
    assert(old.all? { |cursor| cursor.cost.zero? }, 'outer cursor planned before first pull')
    writer.execute('UPDATE oj_c SET v=1001 WHERE id=10')
    fresh = compare_current(enabled, disabled, statements, sql, [1], :query_prepared, trace: true)
    assert(fresh[1] == [[1, 100, 1001]] && old.map(&:rows).uniq == [[[1, 100, 1000]]],
      'outer cursors lost captured publication or new catalog state')
    old.each(&:close)
    early = [enabled, disabled].each_with_index.map { |current, i| current.query_prepared(statements[i], 2) }
    assert(early.map(&:next).uniq == [[2, 200, 2000]], 'outer early-close fixture changed')
    costs = early.map(&:cost)
    early.each(&:close)
    assert(early.map(&:cost) == costs && database.instance_variable_get(:@pins).empty?, 'outer close did more work or retained pins')
    ['ANALYZE oj_b', 'CREATE INDEX oj_b_v ON oj_b(v)', 'DROP INDEX oj_b_v'].each do |action|
      writer.execute(action)
      [1, 2, 3, nil].each { |value| compare_current(enabled, disabled, statements, sql, [value], :query_prepared, trace: true) }
    end
    [enabled, disabled].each { |current| current.timezone = '+02:00' }
    compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    [enabled, disabled].each { |current| current.revoke(:select, 'oj_c') }
    denied = compare_current(enabled, disabled, statements, sql, [99], :query_prepared)
    assert(denied[0] == :error && denied[2] == '42501', 'empty outer join skipped permission revalidation')
    [enabled, disabled].each(&:reset_privileges)
    enabled.begin
    enabled.execute('UPDATE oj_c SET v=1002 WHERE id=10')
    assert(outcome(enabled, statements[0], sql, [1], :query_prepared)[1] == [[1, 100, 1002]], 'outer pending write stayed stale')
    enabled.execute('SAVEPOINT outer_state')
    enabled.execute('UPDATE oj_c SET v=1003 WHERE id=10')
    enabled.execute('ROLLBACK TO outer_state')
    assert(outcome(enabled, statements[0], sql, [1], :query_prepared)[1] == [[1, 100, 1002]], 'outer savepoint rollback stayed stale')
    enabled.rollback
    compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    writer.execute('DROP TABLE oj_c')
    writer.execute('CREATE TABLE oj_c(id i32 PRIMARY KEY,v text)')
    writer.execute("INSERT INTO oj_c VALUES(10,'replacement')")
    changed = compare_current(enabled, disabled, statements, sql, [1], :query_prepared, trace: true)
    assert(changed[1] == [[1, 100, 'replacement']] && changed[3] == %w[i32 i32 text], 'outer schema replacement retained old values/types')
  ensure
    old.to_a.each(&:close)
    early.to_a.each(&:close)
    enabled&.rollback if enabled&.engine&.transaction_open?
    enabled&.close
    disabled&.close
    writer&.close
  end

  def routed_join_kind_mutation
    JedNative.memory do |database|
      setup(database)
      enabled, disabled = session(database), session(database, disabled: true)
      sql = 'SELECT a.id,b.id,c.id FROM oj_a a JOIN oj_b b ON b.id=a.k JOIN oj_c c ON c.id=b.id WHERE a.id=$1 ORDER BY b.id'
      statements = [enabled, disabled].map { |current| current.prepare(sql) }
      2.times { compare_current(enabled, disabled, statements, sql, [1], :query_prepared, no_physical: false) }
      saved = cache(enabled).values.filter_map(&:physical).first
      assert(saved, 'INNER-to-LEFT mutation fixture never retained its old physical winner')
      # Domain routing owns a syntax copy. Mutate the actual planner input to
      # exercise its eligibility guard; public Statement AST mutation is not
      # an advertised API guarantee.
      routed = [enabled, disabled].each_with_index.map do |current, index|
        current.engine.send(:route_domains, statements[index].ast)
      end
      routed.each { |ast| ast[:joins].each { |join| join[:kind] = :left } }
      outer = compare_current(enabled, disabled, statements, sql, [4], :query_prepared, no_physical: false, trace: true)
      assert(outer[1] == [[4, nil, nil]] && @last_counts[0][:planner].positive?,
        "mutated LEFT tree consulted the old INNER physical winner: #{outer.inspect}; #{@last_counts.inspect}")
      assert(cache(enabled).values.any? { |entry| entry.physical.equal?(saved) },
        'mutated LEFT tree replaced the prior INNER physical template')
      routed.each { |ast| ast[:joins].each { |join| join[:kind] = :inner } }
      inner = compare_current(enabled, disabled, statements, sql, [4], :query_prepared, no_physical: false, trace: true)
      assert(inner[1].empty?, 'restored INNER tree retained NULL-extended rows')
    ensure
      enabled&.close
      disabled&.close
    end
  end

  def exclusions_and_bounds
    JedNative.memory do |database|
      enabled = disabled = current = nil
      setup(database)
      exclusions = [
        'SELECT a.id FROM oj_a a RIGHT JOIN oj_b b ON b.k=a.k LEFT JOIN oj_c c ON c.id=b.id',
        'SELECT a.id FROM oj_a a LEFT JOIN oj_b b ON b.k=a.k FULL JOIN oj_c c ON c.id=b.id',
        'SELECT id FROM oj_a LEFT JOIN oj_b USING(id) LEFT JOIN oj_c USING(id)',
        "WITH unused AS(SELECT 1) SELECT a.id #{FROM}",
        "SELECT a.id #{FROM} WHERE EXISTS(SELECT 1)",
        "SELECT count(*) #{FROM}",
        "SELECT DISTINCT a.id #{FROM}",
        "SELECT row_number() OVER(ORDER BY a.id) #{FROM}",
        "SELECT a.id::text #{FROM}",
        "SELECT a.id #{FROM} WHERE 'row' LIKE 'r%'"
      ]
      exclusions.each do |sql|
        enabled, disabled = session(database), session(database, disabled: true)
        statements = [enabled, disabled].map { |current| current.prepare(sql) }
        2.times do
          result = compare_current(enabled, disabled, statements, sql, [], :query_prepared)
          assert(result.first == :rows, "unsupported outer fixture did not execute normally: #{sql}")
        end
        assert(cache(enabled).empty?, "unsupported outer shape entered cache: #{sql}")
        enabled.close
        disabled.close
      end
      current = session(database)
      maximum = JedNative::PreparedJoins::MAX_INPUTS
      [maximum, maximum + 1].each do |width|
        from = "oj_a p0 " + (1...width).map { |i| "LEFT JOIN oj_a p#{i} ON p#{i}.id=p0.id" }.join(' ')
        statement = current.prepare("SELECT p0.id FROM #{from}")
        frame = current.engine.send(:prepared_join_frame, statement.ast)
        assert(!frame.nil? == (width == maximum), 'outer input-count admission bound changed')
        assert(frame.physical_reuse == false && frame.entry.tables.length == width, 'outer frame enabled physical reuse or lost inputs') if frame
      end
      (JedNative::PreparedJoins::CACHE_ENTRIES + 4).times do |index|
        statement = current.prepare("SELECT a.id+#{index},b.id,c.id #{FROM} WHERE a.id=1")
        2.times { current.query_prepared(statement).rows }
      end
      check_bounds(current)
      assert(cache(current).compare_by_identity? && cache(current).length == JedNative::PreparedJoins::CACHE_ENTRIES,
        'outer identity cache failed to reach or enforce its cap')
      current.close
    ensure
      enabled&.close
      disabled&.close
      current&.close
    end
  end

  def fixed_extension_memo
    JedNative.memory do |database|
      setup(database)
      current = session(database)
      statement = current.prepare("SELECT a.id,b.v,c.v #{FROM}")
      current.execute_prepared(statement)
      engine = current.engine
      frame = engine.send(:prepared_join_frame, statement.ast)
      engine.instance_variable_set(:@native_join_resolution_frame, frame)
      context = engine.send(:nway_join_plan, statement.ast[:from], statement.ast[:joins], nil, statement.ast)
      first = engine.send(:nway_driver, context, 0, context[:available][0].first)
      second = first.merge(cost: first[:cost] + 7, rows: first[:rows] + 3, logical: first[:logical] + 5)
      frame.extension_context = frame.extensions = nil
      actual, calls, = traced do
        [first, second].map { |state| engine.send(:nway_extensions, context, state, 1, fixed: true) }
      end
      assert(calls[:extension_requests] == 2 && calls[:extension_resolutions] == 1 && frame.extensions.length == 1,
        'fixed LEFT extension inputs were not reused within one search')
      engine.instance_variable_set(:@native_join_resolution_frame, nil)
      expected = [first, second].map { |state| engine.send(:nway_extensions, context, state, 1, fixed: true) }
      signature = lambda do |choices|
        choices.map do |state|
          [state.values_at(:order, :cost, :rows, :logical, :ordered),
            state[:steps].map { |step| step.slice(:algorithm, :kind, :work, :input_rows, :rows, :logical, :pairs, :prefix_cost, :cost) }]
        end
      end
      assert(actual.map(&signature) == expected.map(&signature), 'fixed LEFT memo changed candidate estimates')
      assert(signature.call(actual[0]) != signature.call(actual[1]), 'fixed LEFT memo retained earlier state estimates')
      indexes = actual.map { |choices| choices.find { |state| state[:steps].last[:algorithm] == :index } }
      assert(indexes.all? && !indexes[0][:steps].last[:index][:access].equal?(indexes[1][:steps].last[:index][:access]),
        'fixed LEFT memo shared mutable index accesses between states')
      engine.instance_variable_set(:@native_join_resolution_frame, frame)
      engine.send(:nway_extensions, context.dup, first, 1, fixed: true)
      assert(!frame.extension_context.equal?(context) && frame.extensions.length == 1,
        'fixed LEFT inputs survived a different planning context')
      engine.instance_variable_set(:@native_join_resolution_frame, nil)
      check_bounds(current)
    ensure
      engine&.instance_variable_set(:@native_join_resolution_frame, nil)
      current&.close
    end
  end

  def column_metadata_ownership
    JedNative.memory do |database|
      setup(database)
      current = session(database)
      statement = current.prepare("SELECT a.id,b.id,c.id #{FROM}")
      engine = current.engine
      frame = engine.send(:prepared_join_frame, statement.ast)
      engine.instance_variable_set(:@native_join_resolution_frame, frame)
      table = engine.instance_variable_get(:@tables).fetch('oj_b')
      name = +'first_alias'
      first = engine.table_columns(table, name)
      assert(engine.table_columns(table, name).equal?(first), 'equal table metadata did not reuse its owned snapshot')
      name.replace('second_alias')
      second = engine.table_columns(table, name)
      assert(first.all? { |column| column[:table] == 'first_alias' && column.frozen? && column[:table].frozen? } &&
        second.all? { |column| column[:table] == 'second_alias' }, 'alias mutation changed previously retained table metadata')
      (JedNative::PreparedJoins::MAX_INPUTS + 4).times { |index| engine.table_columns(table, "alias_#{index}") }
      assert(frame.entry.columns.length == JedNative::PreparedJoins::MAX_INPUTS, 'table metadata alias cache did not enforce its cap')
      engine.instance_variable_set(:@native_join_resolution_frame, nil)
      check_bounds(current)

      # A fresh entry still has room by item count, so rejection here is due to
      # copied-byte limits. A failed snapshot must not consume either allowance.
      other = current.prepare("SELECT a.id+1,b.id,c.id #{FROM}")
      fresh = engine.send(:prepared_join_frame, other.ast)
      engine.instance_variable_set(:@native_join_resolution_frame, fresh)
      huge = 'x' * (JedNative::PreparedJoins::KEY_BYTES + 1)
      before = [fresh.entry.source_units, fresh.entry.source_bytes]
      oversized = engine.table_columns(table, huge)
      again = engine.table_columns(table, huge)
      assert(oversized == again && !oversized.equal?(again) && fresh.entry.columns.empty? &&
        [fresh.entry.source_units, fresh.entry.source_bytes] == before, 'oversized alias was cached or consumed retained metadata budget')
      engine.table_columns(table, 'small')
      assert(fresh.entry.columns.length == 1 && fresh.entry.source_units.positive? && fresh.entry.source_bytes.positive?,
        'oversized alias blocked later bounded metadata')
      # Table descriptors and query-source descriptors share the same allowance.
      fresh.entry.source_units = JedNative::PreparedJoins::KEY_UNITS
      old_size = fresh.entry.columns.length
      first_uncached = engine.table_columns(table, 'exhausted')
      second_uncached = engine.table_columns(table, 'exhausted')
      assert(first_uncached == second_uncached && !first_uncached.equal?(second_uncached) &&
        fresh.entry.columns.length == old_size, 'table metadata ignored the shared source allowance')
      engine.instance_variable_set(:@native_join_resolution_frame, nil)
      current.execute('CREATE INDEX oj_b_v ON oj_b(v)')
      replacement = engine.send(:prepared_join_frame, other.ast)
      assert(!replacement.entry.equal?(fresh.entry) && replacement.entry.columns.empty? &&
        replacement.entry.source_units.zero? && replacement.entry.source_bytes.zero?, 'publication retained exhausted table metadata')
    ensure
      engine&.instance_variable_set(:@native_join_resolution_frame, nil)
      current&.close
    end
  end

  def detached_cache
    JedNative.memory do |database|
      setup(database)
      current = session(database)
      sql = "SELECT a.id,$1,b.label #{FROM} WHERE b.label=$1 ORDER BY a.id,b.id"
      statement = current.prepare(sql)
      parameter = +'twenty'
      2.times { current.query_prepared(statement, parameter).rows }
      cursor = current.query_prepared(statement, parameter)
      execution = cursor.instance_variable_get(:@engine)
      cursor.rows
      cursor.close
      retained = cache(current)
      refs = [current, current.engine, cursor, execution, parameter].map { |object| WeakRef.new(object) }
      current.close
      [retained, refs]
    end
  end

  def run
    [false, true].each do |disk|
      Dir.mktmpdir('jed-outer-reuse') do |directory|
        database = disk ? JedNative.create(File.join(directory, 'fixture.jed'), locking: :exclusive) : JedNative.memory
        setup(database)
        reuse_and_null_extension(database)
        budgets(database)
        invalidation_and_snapshots(database)
      ensure
        database&.close
      end
    end
    routed_join_kind_mutation
    exclusions_and_bounds
    fixed_extension_memo
    column_metadata_ownership
    retained, refs = Thread.new { detached_cache }.value
    3.times { GC.start }
    assert(retained.values.any? { |entry| entry.roots.any? && entry.physical.nil? }, 'outer retention fixture missed admitted roots')
    assert(refs.none?(&:weakref_alive?), 'outer metadata retained session, engine, cursor or binding')
    puts 'ruby-native prepared outer joins: bounded resolution reuse, fresh physical winners, NULL extension, invalidation, snapshots and exact cost prefixes passed'
  end
end

NativePreparedOuterJoinTests.run
