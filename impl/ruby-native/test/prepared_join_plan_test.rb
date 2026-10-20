# frozen_string_literal: true

require 'tmpdir'
require 'weakref'
require_relative '../lib/jed_native'

# The corpus prepares each record separately. These host probes retain the same
# statement while bindings, publications, cursor snapshots and budgets change.
# Only join resolution reuse is disabled in the reference execution.
module NativePreparedJoinPlanTests
  module_function

  APIS = %i[query_prepared execute_prepared query execute].freeze

  def assert(condition, message) = (raise(message) unless condition)

  def setup(database)
    database.execute_script(<<~SQL)
      CREATE TABLE jp_a(id i32 PRIMARY KEY, k i32, narrow i16, flag boolean);
      CREATE TABLE jp_b(id i32 PRIMARY KEY, v i32, bucket i32, flag boolean);
      INSERT INTO jp_a VALUES (1,2,1,TRUE),(2,5,2,NULL),(3,9,3,FALSE),(4,NULL,4,TRUE);
      INSERT INTO jp_b VALUES (2,20,0,TRUE),(4,40,0,FALSE),(5,50,1,NULL);
      CREATE INDEX jp_b_bucket ON jp_b(bucket);
    SQL
  end

  def session(database, disabled: false, **options)
    database.session(**options).tap do |current|
      current.engine.instance_variable_set(:@native_join_resolution_disabled, disabled)
    end
  end

  def cache(current)
    current.engine.instance_variable_get(:@native_join_resolution_plans) || {}
  end

  def invoke(current, statement, sql, params, api)
    api.to_s.end_with?('_prepared') ? current.public_send(api, statement, *params) : current.public_send(api, sql, *params)
  end

  def outcome(current, statement, sql, params, api, collect: false)
    lifetime, phase, values = current.lifetime_cost, :admission, []
    result = invoke(current, statement, sql, params, api)
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
    result.close if result.is_a?(JedNative::Rows)
  end

  def compare_current(enabled, disabled, statements, sql, params, api, **options)
    actual = outcome(enabled, statements[0], sql, params, api, **options)
    expected = outcome(disabled, statements[1], sql, params, api, **options)
    assert(actual == expected, "join reuse changed #{api} #{sql} #{params.inspect}: #{actual.inspect} != #{expected.inspect}")
    actual
  end

  def compare(database, sql, params = [], api: :query_prepared, setting: nil, limit: 0, warm_params: params, **options)
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

  def compile_calls
    count = 0
    trace = TracePoint.new(:call) do |event|
      count += 1 if event.defined_class == JedNative::Engine && event.method_id == :compile
    end
    value = trace.enable { yield }
    [value, count]
  end

  def api_equivalence_and_reuse(database)
    cases = [
      ['SELECT a.id, b.v FROM jp_a a JOIN jp_b b ON b.id = a.k WHERE a.id >= $1 ORDER BY a.id', [[1], [3], [9], [nil], [1]]],
      ['SELECT a.id, b.id FROM jp_a a CROSS JOIN jp_b b WHERE a.k = b.id AND b.v >= $1 ORDER BY a.id, b.id', [[0], [30], [90], [nil], [0]]],
      ['SELECT a.id, b.v FROM jp_a a LEFT JOIN jp_b b ON b.id = a.k AND b.v > $2 WHERE a.id >= $1 ORDER BY a.id', [[1, 0], [1, 30], [3, 0], [1, nil], [1, 0]]],
      ['SELECT a.id, b.id + $1::i32 FROM jp_a a JOIN jp_b b ON b.id = a.id + 1 ORDER BY a.id', [[1], [nil], [-2], [1]]],
      # ON compiles a filter AND; the identical projection is a value AND and
      # must still evaluate its right operand when the left operand is NULL.
      ['SELECT a.id, a.flag AND b.flag FROM jp_a a LEFT JOIN jp_b b ON a.flag AND b.flag ORDER BY a.id, b.id', [[]]]
    ]
    APIS.each do |api|
      cases.each do |sql, bindings|
        enabled, disabled = session(database), session(database, disabled: true)
        statements = [enabled, disabled].map { |current| current.prepare(sql) }
        2.times { bindings.each { |params| compare_current(enabled, disabled, statements, sql, params, api) } }
        assert(cache(enabled).any? && cache(enabled).values.any? { |entry| entry.roots.any? }, "#{api} did not exercise join resolution reuse")
        assert(cache(disabled).empty?, 'disabled join resolution retained an entry')
        enabled.close
        disabled.close
      end
    end

    enabled, disabled = session(database), session(database, disabled: true)
    sql = cases.first.first
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    3.times { compare_current(enabled, disabled, statements, sql, [1], :query_prepared) }
    entry = cache(enabled).values.first
    _, reused = compile_calls { outcome(enabled, statements[0], sql, [3], :query_prepared) }
    _, ordinary = compile_calls { outcome(disabled, statements[1], sql, [3], :query_prepared) }
    assert(reused < ordinary && ordinary.positive?, "warm join repeated all resolution (#{reused} versus #{ordinary})")
    assert(cache(enabled).values.first.equal?(entry), 'changing an ordinary binding replaced the resolved entry')
    enabled.engine.instance_variable_set(:@native_join_resolution_disabled, true)
    _, bypassed = compile_calls { outcome(enabled, statements[0], sql, [3], :query_prepared) }
    assert(bypassed == ordinary, 'internal bypass continued to use an already-warmed entry')
    enabled.engine.instance_variable_set(:@native_join_resolution_disabled, false)
    compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
  ensure
    enabled&.close
    disabled&.close
  end

  def contextual_checks(database)
    enabled = disabled = nil
    cases = [
      ['SELECT a.narrow + $1, b.v FROM jp_a a JOIN jp_b b ON b.id = a.k WHERE a.id = $2 ORDER BY a.id', [1, 1], [[32_768, 1], [32_768, 99], [-32_769, 99], [nil, 1], [2, 1]]],
      ['SELECT a.id FROM jp_a a JOIN jp_b b ON b.id = a.k WHERE a.narrow + $1 > 0 AND b.v + $2 > 0 ORDER BY a.id', [1, 1], [[32_768, 2**31], [1, 2**31], [nil, nil], [1, 1]]],
      ['SELECT a.id, b.v / $1 FROM jp_a a JOIN jp_b b ON b.id = a.k WHERE a.id >= $2 ORDER BY a.id', [2, 1], [[0, 1], [0, 99], [nil, 1], [2, 1]]]
    ]
    APIS.each do |api|
      cases.each do |sql, initial, bindings|
        enabled, disabled = session(database), session(database, disabled: true)
        statements = [enabled, disabled].map { |current| current.prepare(sql) }
        2.times do
          warmed = compare_current(enabled, disabled, statements, sql, initial, api)
          assert(warmed[0] == :rows, 'contextual parameter fixture failed before warming')
        end
        bindings.each { |params| compare_current(enabled, disabled, statements, sql, params, api) }
        enabled.close
        disabled.close
      end
    end
  ensure
    enabled&.close
    disabled&.close
  end

  def publication_and_transactions(database)
    enabled, disabled, writer = session(database), session(database, disabled: true), database.session
    sql = 'SELECT a.id, b.v FROM jp_a a JOIN jp_b b ON b.id = a.k WHERE a.id >= $1 ORDER BY a.id'
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    2.times { compare_current(enabled, disabled, statements, sql, [1], :query_prepared) }
    entry = cache(enabled).values.first
    writer.execute('CREATE TABLE jp_unrelated(id i32)')
    compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    # A publication can conservatively replace descriptors of untouched tables;
    # either a hit or a re-resolution must observe the same rows and costs.
    writer.execute('INSERT INTO jp_unrelated VALUES(1)')
    compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    writer.execute('UPDATE jp_b SET v = 21 WHERE id = 2')
    changed = compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    assert(changed[1].first == [1, 21] && !cache(enabled).values.first.equal?(entry), 'same-row-count publication retained old join state')

    enabled.begin
    enabled.execute('UPDATE jp_b SET v = 31 WHERE id = 2')
    assert(outcome(enabled, statements[0], sql, [1], :query_prepared)[1].first == [1, 31], 'pending join did not see own write')
    enabled.execute('SAVEPOINT join_before_change')
    enabled.execute('UPDATE jp_b SET v = 41 WHERE id = 2')
    assert(outcome(enabled, statements[0], sql, [1], :query_prepared)[1].first == [1, 41], 'savepoint join retained earlier pending value')
    enabled.execute('ROLLBACK TO join_before_change')
    assert(outcome(enabled, statements[0], sql, [1], :query_prepared)[1].first == [1, 31], 'savepoint rollback retained later pending value')
    enabled.rollback
    compare_current(enabled, disabled, statements, sql, [1], :query_prepared)

    [enabled, disabled].each { |current| current.revoke(:select, 'jp_b') }
    denied = compare_current(enabled, disabled, statements, sql, [99], :query_prepared)
    assert(denied[0] == :error && denied[2] == '42501', 'warm empty join skipped permission admission')
    [enabled, disabled].each(&:reset_privileges)
    compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    old_entry = cache(enabled).values.first
    [enabled, disabled].each { |current| current.timezone = '+02:00' }
    compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    assert(!cache(enabled).values.first.equal?(old_entry), 'session context change retained a context-specific entry')

    writer.execute('DROP TABLE jp_b')
    writer.execute('CREATE TABLE jp_b(id i32 PRIMARY KEY, v text, bucket i32, flag boolean)')
    writer.execute("INSERT INTO jp_b VALUES(2,'replacement',0,TRUE)")
    replaced = compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    assert(replaced[1] == [[1, 'replacement']] && replaced[3] == %w[i32 text], 'replacement table retained old ordinal/type metadata')
  ensure
    enabled&.rollback if enabled&.engine&.transaction_open?
    enabled&.close
    disabled&.close
    writer&.close
  end

  def parameter_sensitive_planning(database)
    writer = database.session
    writer.execute_script(<<~SQL)
      CREATE TABLE jp_outer(id i32 PRIMARY KEY, k i32);
      CREATE TABLE jp_skew(id i32 PRIMARY KEY, bucket i32);
      CREATE INDEX jp_skew_bucket ON jp_skew(bucket);
    SQL
    writer.execute("INSERT INTO jp_outer VALUES #{(1..12).map { |id| "(#{id},#{id})" }.join(',')}")
    writer.execute("INSERT INTO jp_skew VALUES #{(1..40).map { |id| "(#{id},#{id <= 35 ? 0 : id})" }.join(',')}")
    enabled, disabled = session(database), session(database, disabled: true)
    sql = 'SELECT a.id, b.id FROM jp_outer a JOIN jp_skew b ON a.k = b.id WHERE b.bucket = $1 ORDER BY a.id LIMIT 10'
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    actions = [nil, 'ANALYZE jp_skew', 'UPDATE jp_skew SET bucket = 0 WHERE id = 36',
      'CREATE INDEX jp_skew_same_key ON jp_skew(id)', 'DROP INDEX jp_skew_same_key', 'DELETE FROM jp_skew WHERE id = 2']
    actions.each do |action|
      writer.execute(action) if action
      2.times do
        [0, 36, 90, nil, 0].each { |binding| compare_current(enabled, disabled, statements, sql, [binding], :query_prepared) }
      end
    end
  ensure
    enabled&.close
    disabled&.close
    writer&.close
  end

  def cursor_snapshots(database)
    enabled, disabled, writer = session(database), session(database, disabled: true), database.session
    sql = 'SELECT a.id + $1::i32, b.v FROM jp_a a JOIN jp_b b ON b.id = a.k WHERE a.id >= $2 ORDER BY a.id'
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    2.times { compare_current(enabled, disabled, statements, sql, [0, 1], :query_prepared) }
    first = [enabled, disabled].each_with_index.map { |current, index| current.query_prepared(statements[index], 1, 1) }
    second = [enabled, disabled].each_with_index.map { |current, index| current.query_prepared(statements[index], 10, 2) }
    assert((first + second).all? { |cursor| cursor.cost.zero? }, 'join cursor performed work before first pull')
    writer.execute('UPDATE jp_b SET v = 99 WHERE id = 5')
    compare_current(enabled, disabled, statements, sql, [100, 1], :query_prepared)
    assert(second.map(&:rows).uniq == [[[12, 50]]], 'interleaved cursor lost its old parameter or snapshot')
    assert(first.map(&:rows).uniq == [[[2, 20], [3, 50]]], 'first cursor lost its old parameter or snapshot')
    # Early close must not revisit the remainder or retain the shared pin.
    cursors = [enabled, disabled].each_with_index.map { |current, index| current.query_prepared(statements[index], 0, 1) }
    rows = cursors.map(&:next)
    assert(rows == [[1, 20], [1, 20]], 'warm cursor first row differs')
    costs = cursors.map(&:cost)
    cursors.each(&:close)
    assert(cursors.map(&:cost) == costs && database.instance_variable_get(:@pins).empty?, 'closing a warm join did more work or retained a pin')
  ensure
    (first.to_a + second.to_a + cursors.to_a).each(&:close)
    enabled&.close
    disabled&.close
    writer&.close
  end

  def budgets(database)
    sql = 'SELECT a.id, b.v / $1 FROM jp_a a JOIN jp_b b ON b.id = a.k WHERE a.id >= $2 ORDER BY a.id'
    %i[query_prepared execute_prepared].each do |api|
      [[2, 1], [0, 1], [0, 99]].each do |params|
        full = compare(database, sql, params, api: api, warm_params: [2, 1])
        total = full[-2]
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
  end

  def fallback_and_bounds
    enabled = disabled = current = cursor = bounded = nil
    calls = []
    environment = JedNative::Environment.new.register_function(name: 'jp_probe', args: ['i32'], returns: 'i32') { |value| calls << value; value }
    JedNative.memory(environment: environment) do |database|
      setup(database)
      unsupported = [
        'SELECT count(*) FROM jp_a a JOIN jp_b b ON a.k = b.id',
        'SELECT a.id, row_number() OVER (ORDER BY a.id) FROM jp_a a JOIN jp_b b ON a.k = b.id',
        'SELECT a.id FROM jp_a a JOIN jp_b b ON a.k = b.id WHERE EXISTS(SELECT 1)',
        'WITH q AS (SELECT 1 AS n) SELECT a.id FROM jp_a a JOIN jp_b b ON a.k = b.id',
        'SELECT a.id::text FROM jp_a a JOIN jp_b b ON a.k = b.id',
        "SELECT a.id FROM jp_a a JOIN jp_b b ON a.k = b.id WHERE 'row' LIKE 'r%'",
        'SELECT a.id FROM jp_a a JOIN jp_b b ON a.k = b.id JOIN jp_a c ON c.id = a.id',
        'SELECT a.id FROM jp_a a RIGHT JOIN jp_b b ON a.k = b.id'
      ]
      unsupported.each do |sql|
        enabled, disabled = session(database), session(database, disabled: true)
        statements = [enabled, disabled].map { |current| current.prepare(sql) }
        2.times { compare_current(enabled, disabled, statements, sql, [], :query_prepared) }
        assert(cache(enabled).empty?, "unsupported join entered resolution cache: #{sql}")
        enabled.close
        disabled.close
      end
      current = session(database)
      sql = 'SELECT jp_probe(a.id) FROM jp_a a JOIN jp_b b ON a.k = b.id ORDER BY a.id'
      prepared = current.prepare(sql)
      2.times { current.query_prepared(prepared).rows }
      calls.clear
      cursor = current.query_prepared(prepared)
      assert(calls.empty? && cursor.cost.zero?, 'unsupported host projection ran during cursor creation')
      assert(cursor.rows == [[1], [2]] && calls == [1, 2] && cache(current).empty?, 'host fallback cached an invocation or admitted a host expression')

      70.times do |index|
        query = "SELECT a.id + #{index}, b.v FROM jp_a a JOIN jp_b b ON a.k = b.id WHERE a.id = 1"
        statement = current.prepare(query)
        2.times { current.query_prepared(statement).rows }
      end
      assert(cache(current).compare_by_identity? && cache(current).length == 64, 'join statement cache is not a bounded identity map')
      assert(cache(current).values.all? { |entry| entry.roots.length <= 128 }, 'per-statement join resolution cache exceeded its root bound')
      assert(cache(current).values.all? { |entry| entry.key_units <= 4096 && entry.key_bytes <= 65_536 }, 'retained resolution keys exceeded their aggregate size bound')
      # Admission limits are cumulative across the statement, before retention.
      ["SELECT '#{'x' * 32_769}' FROM jp_a a JOIN jp_b b ON a.k = b.id"].each do |query|
        bounded = session(database)
        prepared = bounded.prepare(query)
        assert(bounded.query_prepared(prepared).rows.length == 2, 'over-budget query did not fall back')
        assert(cache(bounded).empty?, 'oversized SQL value entered join cache')
        bounded.close
      end
      current.close
    end
  ensure
    cursor&.close
    current&.close
    enabled&.close
    disabled&.close
    bounded&.close
  end

  def compile_root_invariants
    JedNative.memory do |database|
      current = session(database)
      engine = current.engine
      entry = JedNative::PreparedJoins::Entry.new(tables: [], context: [], roots: {}, columns: {}, key_units: 0, key_bytes: 0)
      frame = JedNative::PreparedJoins::Frame.new(entry: entry, depth: 0, plans: {})
      engine.instance_variable_set(:@native_join_resolution_frame, frame)
      columns = [{ name: 'x', type: 'boolean' }, { name: 'y', type: 'boolean' }]
      node = { kind: :binary, op: 'and', left: { kind: :column, name: 'x' }, right: { kind: :column, name: 'y' } }
      predicate = engine.compile_filter(node, columns)
      value = engine.compile(node, columns)
      assert(!predicate.equal?(value), 'value and filter roots alias the same mutable expression')
      assert(engine.evaluate(value, [nil, false]) == false && engine.evaluate(predicate, [nil, false]).nil?,
        'cached filter marking changed value AND semantics')
      assert(engine.compile_filter(node, columns).equal?(predicate) && engine.compile(node, columns).equal?(value), 'mode-specific roots did not reuse')

      # Binding inference rejects conflicting public parameter contexts. Probe
      # the lower-level resolution seam directly so its replay is independently
      # checked for narrowing, left-to-right errors and an empty access result.
      columns = [{ name: 'small', type: 'i16' }, { name: 'large', type: 'i32' }]
      left = { kind: :binary, op: '+', left: { kind: :column, name: 'small' }, right: { kind: :param, index: 1 } }
      right = { kind: :binary, op: '+', left: { kind: :column, name: 'large' }, right: { kind: :param, index: 2 } }
      node = { kind: :binary, op: '+', left: left, right: right }
      engine.instance_variable_set(:@native_binding_types, %w[i64 i64])
      engine.instance_variable_set(:@params, [1, 1])
      resolved = engine.compile(node, columns)
      checks = entry.roots.values.find { |expression, _| expression.equal?(resolved) }&.last
      assert(checks&.chunk(&:itself)&.map(&:first) == [[0, 'i16'], [1, 'i32']],
        "contextual narrowing checks were not recorded in expression order: #{checks.inspect}")
      [[32_768, 2**31], [1, 2**31], [-32_769, 1], [nil, nil], [2, 3]].each do |params|
        engine.instance_variable_set(:@params, params)
        results = [frame, nil].map do |active|
          engine.instance_variable_set(:@native_join_resolution_frame, active)
          before = engine.cost
          begin
            expression = engine.compile(node, columns)
            [:ok, expression.type, engine.cost - before]
          rescue JedNative::Error => error
            [error.sqlstate, error.message, engine.cost - before]
          end
        end
        assert(results.first == results.last, "cached contextual replay changed #{params.inspect}: #{results.inspect}")
      end
      assert(frame.depth.zero? && frame.checks.nil?, 'failed contextual replay left a resolution frame active')
      current.close
    end
  end

  def syntax_boundaries
    JedNative.memory do |database|
      setup(database)
      cases = [
        ["SELECT #{Array.new(189, 'a.id').join(',')} FROM jp_a a JOIN jp_b b ON a.k = b.id", true],
        ["SELECT #{Array.new(190, 'a.id').join(',')} FROM jp_a a JOIN jp_b b ON a.k = b.id", false],
        ["SELECT #{'not ' * 32}a.flag FROM jp_a a JOIN jp_b b ON a.k = b.id", true],
        ["SELECT #{'not ' * 33}a.flag FROM jp_a a JOIN jp_b b ON a.k = b.id", false]
      ]
      cases.each_with_index do |(sql, admitted), index|
        current = session(database)
        prepared = current.prepare(sql)
        result = outcome(current, prepared, sql, [], :query_prepared)
        assert(result.first == :rows && result[1].length == 2, 'bounded syntax did not execute through its ordinary fallback')
        assert(cache(current).any? == admitted, "join syntax node/depth boundary #{index} changed: admitted=#{admitted}")
        current.close
      end
      [32, 33].each do |width|
        columns = (0...width).map { |index| "c#{index} i32#{index.zero? ? ' PRIMARY KEY' : ''}" }.join(',')
        database.execute("CREATE TABLE jp_width#{width}(#{columns})")
      end
      [32, 33].each do |right_width|
        sql = "SELECT a.c0 FROM jp_width32 a JOIN jp_width#{right_width} b ON a.c0 = b.c0"
        current = session(database)
        assert(current.query(sql).rows.empty?, 'empty wide join changed its result')
        assert(cache(current).any? == (right_width == 32), 'join input-column boundary changed')
        current.close
      end
    end
  end

  def detached_cache
    JedNative.memory do |database|
      database.execute_script('CREATE TABLE jp_hold_a(id i32 PRIMARY KEY, label text); CREATE TABLE jp_hold_b(id i32 PRIMARY KEY); ' \
        "INSERT INTO jp_hold_a VALUES(1,'captured'); INSERT INTO jp_hold_b VALUES(1)")
      current = session(database)
      statement = current.prepare('SELECT a.id, $1 FROM jp_hold_a a JOIN jp_hold_b b ON a.id = b.id WHERE a.label = $1')
      input = +'captured'
      2.times { current.query_prepared(statement, input).rows }
      result = current.query_prepared(statement, input)
      execution = result.instance_variable_get(:@engine)
      result.rows
      retained = cache(current)
      references = [current, current.engine, result, execution, input].map { |object| WeakRef.new(object) }
      current.close
      [retained, references]
    end
  end

  def retention
    # A finished thread avoids conservative stale VM argument slots looking
    # like reachability from the retained resolution metadata.
    retained, references = Thread.new { detached_cache }.value
    assert(retained.any? && retained.values.any? { |entry| entry.roots.any? }, 'retention probe did not warm a join cache')
    3.times { GC.start }
    assert(references.none?(&:weakref_alive?), 'join metadata retained a session, engine, cursor or bound host value')
  end

  def run
    %i[memory file].each do |mode|
      runner = lambda do |database|
        setup(database)
        api_equivalence_and_reuse(database)
        contextual_checks(database)
        budgets(database)
        cursor_snapshots(database)
        parameter_sensitive_planning(database)
        publication_and_transactions(database)
      end
      if mode == :memory
        JedNative.memory(&runner)
      else
        Dir.mktmpdir('jed-ruby-prepared-join') do |directory|
          JedNative.create(File.join(directory, 'db'), page_size: 512, locking: :exclusive, &runner)
        end
      end
    end
    fallback_and_bounds
    compile_root_invariants
    syntax_boundaries
    retention
    puts 'ruby-native prepared joins: bounded resolution reuse, binding-sensitive planning, contextual checks, snapshots, invalidation and exact budget prefixes passed'
  end
end

NativePreparedJoinPlanTests.run
