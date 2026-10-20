# frozen_string_literal: true

require 'tmpdir'
require 'weakref'
require_relative '../lib/jed_native'

# Shared transcripts own function semantics. These host probes retain JOIN
# statements and observe resolution reuse, execution state and invalidation.
module NativePreparedJoinFunctionTests
  APIS = %i[query_prepared execute_prepared query execute].freeze
  JOIN = 'FROM jf_a a JOIN jf_b b ON b.id=a.k'
  module_function

  def assert(value, message) = (raise message unless value)

  def setup(database)
    database.execute_script(<<~SQL)
      CREATE TABLE jf_a(id i32 PRIMARY KEY,k i32,narrow i16,label text,flag boolean);
      CREATE TABLE jf_b(id i32 PRIMARY KEY,k i32,v i32,label text);
      CREATE TABLE jf_c(id i32 PRIMARY KEY,v i32,label text);
      INSERT INTO jf_a VALUES(1,10,-3,'Alpha',TRUE),(2,20,NULL,'Éclair',NULL),(3,30,-32768,'',FALSE),(4,NULL,4,NULL,TRUE);
      INSERT INTO jf_b VALUES(10,10,-7,'alpha'),(20,20,0,'Beta'),(30,NULL,9,''),(40,99,-2147483648,'Z');
      INSERT INTO jf_c VALUES(10,2,'ten'),(20,3,'twenty'),(30,4,'empty');
      CREATE INDEX jf_b_k ON jf_b(k);
    SQL
  end

  def session(database, disabled: false)
    database.session.tap do |current|
      current.engine.instance_variable_set(:@native_join_resolution_disabled, disabled)
      # Prove resolution hits independently of the older physical-plan cache.
      current.engine.instance_variable_set(:@native_join_physical_disabled, true)
    end
  end

  def cache(current) = current.engine.instance_variable_get(:@native_join_resolution_plans) || {}

  def metadata(value)
    case value
    when Array then value.map { |item| metadata(item) }
    when JedNative::CollatedText then [value.class.name, value.to_s, value.collation_label&.to_h]
    when String then [value.class.name, value.to_s]
    else value
    end
  end

  def outcome(current, statement, sql, params, api, collect: false)
    prior, phase, values, checkpoints = current.lifetime_cost, :admission, [], []
    result = api.to_s.end_with?('_prepared') ? current.public_send(api, statement, *params) : current.public_send(api, sql, *params)
    names, types = result.columns, result.column_types
    phase = :pull
    if result.is_a?(JedNative::Rows) && !collect
      result.each do |row|
        values << metadata(row.to_a)
        checkpoints << [result.cost, current.lifetime_cost - prior]
      end
    else
      values = metadata(result.rows)
    end
    [:rows, values, names, types, checkpoints, result.cost, current.lifetime_cost - prior]
  rescue JedNative::Error => error
    [:error, phase, error.sqlstate, error.message, values, checkpoints,
     result&.cost || current.engine.cost, current.lifetime_cost - prior]
  ensure
    result&.close
  end

  def traced
    counts = Hash.new(0)
    trace = TracePoint.new(:call) do |event|
      counts[:compile] += 1 if event.defined_class == JedNative::Engine && event.method_id == :compile
      counts[:physical] += 1 if %i[two_base_join_plan nway_join_plan].include?(event.method_id) &&
                               [JedNative::JoinPlanner, JedNative::NWayJoins].include?(event.defined_class)
    end
    value = trace.enable { yield }
    [value, counts]
  ensure
    trace&.disable
  end

  def check_bounds(current)
    assert(cache(current).length <= JedNative::PreparedJoins::CACHE_ENTRIES, 'function statements exceeded the JOIN cache limit')
    cache(current).each_value do |entry|
      assert(entry.roots.length <= JedNative::PreparedJoins::ROOT_ENTRIES &&
        entry.key_units <= JedNative::PreparedJoins::KEY_UNITS && entry.key_bytes <= JedNative::PreparedJoins::KEY_BYTES,
        'function roots exceeded the existing metadata allowance')
      assert(entry.nodes.keys.all?(&:frozen?) && entry.scopes.keys.all?(&:frozen?),
        'function resolution retained mutable syntax/scope keys')
    end
    assert(current.engine.instance_variable_get(:@native_join_resolution_frame).nil?, 'function resolution frame escaped its SELECT')
  end

  def compare_current(enabled, disabled, statements, sql, params, api, **options)
    actual, used = traced { outcome(enabled, statements[0], sql, params, api, **options) }
    expected, original = traced { outcome(disabled, statements[1], sql, params, api, **options) }
    assert(actual == expected, "function reuse changed #{api} #{sql} #{params.inspect}: #{actual.inspect} != #{expected.inspect}")
    assert(cache(disabled).empty?, 'disabled function resolution retained an entry')
    [enabled, disabled].each { |current| check_bounds(current) }
    @last_counts = [used, original]
    actual
  end

  def pair(database, sql)
    enabled, disabled = session(database), session(database, disabled: true)
    [enabled, disabled, [enabled.prepare(sql), disabled.prepare(sql)]]
  end

  def compare(database, sql, params, api:, warm: params, setting: nil, limit: 0, **options)
    enabled, disabled, statements = pair(database, sql)
    2.times { compare_current(enabled, disabled, statements, sql, warm, api, **options) }
    [enabled, disabled].each do |current|
      current.public_send("#{setting}=", setting == :lifetime_max_cost ? current.lifetime_cost + limit : limit) if setting
    end
    compare_current(enabled, disabled, statements, sql, params, api, **options)
  ensure
    enabled&.close
    disabled&.close
  end

  def family_reuse(database)
    enabled = disabled = nil
    cases = [
      ["SELECT a.id,abs(b.v) #{JOIN} WHERE a.id>=$1 ORDER BY a.id", [[1], [2], [nil], [1]]],
      ["SELECT a.id,mod(b.v,$1::i32),gcd(a.id,$1::i32),lcm(a.id,$1::i32) #{JOIN} ORDER BY a.id", [[2], [3], [nil], [2]]],
      ["SELECT a.id,length(a.label),lower(a.label),upper(b.label) #{JOIN} WHERE a.id>=$1 ORDER BY a.id", [[1], [2], [nil], [1]]],
      ["SELECT a.id,char_length(a.label),character_length(a.label),octet_length(a.label),bit_length(a.label),ascii(a.label),chr(a.id+64) #{JOIN} WHERE a.id>=$1 ORDER BY a.id",
        [[1], [2], [nil], [1]]],
      ["SELECT a.id,initcap(a.label),reverse(a.label),left(a.label,$1),right(a.label,$1),substr(a.label,1,$1) #{JOIN} ORDER BY a.id",
        [[2], [0], [nil], [2]]],
      ["SELECT a.id,btrim(a.label,$1),ltrim(a.label,$1),rtrim(a.label,$1),replace(a.label,$1,'x'),translate(a.label,'Al','xy'),split_part(a.label,'l',1),strpos(a.label,$1),starts_with(a.label,$1) #{JOIN} ORDER BY a.id",
        [['A'], ['l'], [nil], ['A']]],
      ['SELECT a.id,b.id FROM jf_a a JOIN jf_b b ON b.id=abs(a.k) AND abs(b.v-a.id)<$1 ORDER BY a.id,b.id', [[10], [1], [nil], [10]]],
      ["SELECT a.id,b.id #{JOIN} WHERE length(a.label)>$1 ORDER BY abs(b.v),a.id", [[0], [4], [nil], [0]]],
      ['SELECT a.id,lower(c.label),abs(b.v+c.v) FROM jf_a a JOIN jf_b b ON b.id=abs(a.k) JOIN jf_c c ON c.id=b.id WHERE a.id>=$1 ORDER BY a.id',
        [[1], [2], [nil], [1]]],
      ['SELECT a.id,lower(b.label),abs(c.v) FROM jf_a a LEFT JOIN jf_b b ON b.id=abs(a.k) LEFT JOIN jf_c c ON c.id=b.id WHERE a.id>=$1 ORDER BY a.id',
        [[1], [3], [nil], [1]]]
    ]
    APIS.each do |api|
      cases.each do |sql, bindings|
        enabled, disabled, statements = pair(database, sql)
        bindings.each { |params| compare_current(enabled, disabled, statements, sql, params, api) }
        actual = compare_current(enabled, disabled, statements, sql, bindings.first, api)
        used, original = @last_counts
        assert(actual.first == :rows && cache(enabled).values.any? { |entry| entry.roots.any? },
          "builtin fixture failed to admit resolution #{api}: #{sql}: #{actual.inspect}")
        assert(used[:compile] < original[:compile] && original[:compile].positive?,
          "warm builtin resolution did not avoid compilation #{api}: #{sql}: #{@last_counts.inspect}")
        enabled.engine.instance_variable_set(:@native_join_resolution_disabled, true)
        _, bypass = traced { outcome(enabled, statements[0], sql, bindings.first, api) }
        assert(bypass[:compile] == original[:compile], 'disable did not bypass warmed function roots')
        enabled.close
        disabled.close
      end
    end
  ensure
    enabled&.close
    disabled&.close
  end

  def error_prefixes(database)
    cases = [
      ["SELECT a.id,abs(a.narrow) #{JOIN} WHERE a.id=$1", [3], [1], '22003'],
      ["SELECT a.id,abs(b.v)/$1 #{JOIN} WHERE a.id=1", [0], [2], '22012'],
      ["SELECT a.id,abs(a.narrow+$1) #{JOIN} WHERE a.id=$2", [40_000, 1], [1, 1], '22003']
    ]
    APIS.each do |api|
      cases.each do |sql, params, warm, code|
        answer = compare(database, sql, params, api: api, warm: warm)
        assert(answer[0] == :error && answer[2] == code, "function error fixture did not raise #{code}: #{answer.inspect}")
        # Preserve every statement/lifetime prefix, including which error wins.
        (1..answer[-2] + 1).each do |limit|
          %i[max_cost lifetime_max_cost].each do |setting|
            compare(database, sql, params, api: api, warm: warm, setting: setting, limit: limit)
          end
        end
      end
      null = compare(database, "SELECT a.id,abs(a.narrow) #{JOIN} WHERE a.id=$1", [2], api: api, warm: [1])
      assert(null[0] == :rows && null[1] == [[2, nil]], 'abs NULL changed after warming an integer result')
      empty = compare(database, "SELECT a.id,abs(a.narrow) #{JOIN} WHERE a.id=$1", [99], api: api, warm: [1])
      assert(empty[0] == :rows && empty[1].empty?, 'function overflow was evaluated for an absent row')
    end
  end

  def publications_and_snapshots(database)
    enabled, disabled, statements = pair(database,
      "SELECT a.id,abs(b.v),lower(b.label) #{JOIN} WHERE a.id>=$1 ORDER BY a.id")
    writer = database.session
    sql = "SELECT a.id,abs(b.v),lower(b.label) #{JOIN} WHERE a.id>=$1 ORDER BY a.id"
    2.times { compare_current(enabled, disabled, statements, sql, [1], :query_prepared) }
    before = cache(enabled).values.first
    old = [enabled, disabled].each_with_index.map { |current, index| current.query_prepared(statements[index], 1) }
    later = [enabled, disabled].each_with_index.map { |current, index| current.query_prepared(statements[index], 2) }
    assert((old + later).all? { |cursor| cursor.cost.zero? }, 'function cursor evaluated work before first pull')
    writer.execute("UPDATE jf_b SET v=-11,label='NEW' WHERE id=10")
    changed = compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    assert(changed[1].first[0, 2] == [1, 11] && !cache(enabled).values.first.equal?(before),
      'function root retained a prior table publication')
    old_rows, later_rows = old.map(&:rows), later.map(&:rows)
    assert(old_rows.uniq.length == 1 && later_rows.uniq.length == 1,
      'function roots changed an admitted snapshot or its binding')
    assert(old_rows.first.first[0, 2] == [1, 7] && later_rows.first.first.first == 2,
      'snapshot fixture did not retain the old values/parameters')

    before = cache(enabled).values.first
    writer.execute('ANALYZE jf_b')
    compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    assert(!cache(enabled).values.first.equal?(before), 'ANALYZE retained stale function resolution context')
    [nil, 2, 99, 1].each { |value| compare_current(enabled, disabled, statements, sql, [value], :query_prepared) }
    explain = "EXPLAIN (VERBOSE) #{sql}"
    explained = [enabled.prepare(explain), disabled.prepare(explain)]
    [1, 3, 99, nil].each { |value| compare_current(enabled, disabled, explained, explain, [value], :query_prepared) }

    before = cache(enabled).values.first
    [enabled, disabled].each { |current| current.timezone = '+02:00' }
    compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    assert(!cache(enabled).values.first.equal?(before), 'context change reused an old function root')
    admitted = enabled.query_prepared(statements[0], 1)
    enabled.revoke(:select, 'jf_b')
    denied = outcome(enabled, statements[0], sql, [1], :query_prepared)
    assert(denied[0] == :error && denied[2] == '42501' && admitted.rows.first[0, 2] == [1, 11],
      'function reuse bypassed current capabilities or changed captured capabilities')
    enabled.reset_privileges

    captured = [enabled, disabled].each_with_index.map { |current, index| current.query_prepared(statements[index], 1) }
    writer.execute('DROP TABLE jf_b')
    writer.execute('CREATE TABLE jf_b(id i32 PRIMARY KEY,k i32,v i16,label text)')
    writer.execute("INSERT INTO jf_b VALUES(10,10,-12,'changed')")
    fresh = compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    assert(fresh[3] == %w[i32 i16 text] && fresh[1].first[0, 2] == [1, 12],
      'function reuse retained an earlier argument/result type')
    assert(captured.map(&:rows).uniq.length == 1 && captured.first.column_types == %w[i32 i32 text],
      'schema replacement changed an admitted function cursor')
    [old, later, captured].flatten.each(&:close)
    assert(database.instance_variable_get(:@pins).empty?, 'function cursors retained storage pins after drain')
  ensure
    [*old.to_a, *later.to_a, *captured.to_a, admitted, enabled, disabled, writer].each { |value| value&.close }
  end

  def host_fences
    enabled = disabled = nil
    calls = []
    environment = JedNative::Environment.new
      .register_function(name: 'abs', args: ['i32'], returns: 'i32') { |value| calls << [:integer, value]; 999 }
      .register_function(name: 'abs', args: ['text'], returns: 'text') { |value| calls << [:text, value]; value&.reverse }
      .register_function(name: 'lower', args: ['text'], returns: 'text') { |value| calls << [:lower, value]; 'HOST' }
    JedNative.memory(environment: environment) do |database|
      setup(database)
      [
        ["SELECT a.id,abs(b.v),lower(b.label) #{JOIN} ORDER BY a.id", false],
        ["SELECT a.id,abs(a.label) #{JOIN} ORDER BY a.id", true]
      ].each do |sql, host|
        APIS.each do |api|
          enabled, disabled, statements = pair(database, sql)
          3.times do
            calls.clear
            actual = outcome(enabled, statements[0], sql, [], api)
            observed = calls.dup
            calls.clear
            expected = outcome(disabled, statements[1], sql, [], api)
            assert(actual == expected && observed == calls, 'host overload changed values, cost or callback order')
            assert(cache(enabled).empty?, 'same-name host registry entered builtin resolution reuse')
            assert(host ? observed.any? && observed.all? { |kind, _| kind == :text } : observed.empty?,
              'builtin precedence or host fallback changed')
          end
          enabled.close
          disabled.close
        end
      end
    end
  ensure
    enabled&.close
    disabled&.close
  end

  def rejected_calls(database)
    enabled = disabled = nil
    [
      "SELECT coalesce(b.v,0),nullif(b.v,0),greatest(a.id,b.v),least(a.id,b.v) #{JOIN}",
      "SELECT count(*) #{JOIN}",
      "SELECT row_number() OVER (ORDER BY a.id) #{JOIN}",
      "SELECT abs(a.id) FILTER (WHERE TRUE) #{JOIN}",
      "SELECT abs(DISTINCT a.id) #{JOIN}",
      "SELECT abs(a.id ORDER BY a.id) #{JOIN}",
      "SELECT abs(a.id) OVER () #{JOIN}",
      "SELECT trim(a.label) #{JOIN}",
      "SELECT repeat(a.label,2) #{JOIN}",
      "SELECT abs(a.id::decimal) #{JOIN}"
    ].each do |sql|
      enabled, disabled, statements = pair(database, sql)
      2.times { compare_current(enabled, disabled, statements, sql, [], :query_prepared) }
      assert(cache(enabled).empty?, "unsupported call admitted builtin JOIN roots: #{sql}")
      enabled.close
      disabled.close
    end
  ensure
    enabled&.close
    disabled&.close
  end

  def fresh_frame
    entry = JedNative::PreparedJoins::Entry.new(tables: [], context: [], roots: {}, columns: {},
      scopes: {}, nodes: {}, key_units: 0, key_bytes: 0, sources: {}, source_units: 0, source_bytes: 0)
    JedNative::PreparedJoins::Frame.new(entry: entry, depth: 0, plans: {}, physical_reuse: true)
  end

  def root_invariants
    JedNative.memory do |database|
      current = session(database)
      engine, frame = current.engine, fresh_frame
      engine.instance_variable_set(:@native_join_resolution_frame, frame)
      columns = [{ name: 'small', type: 'i16' }, { name: 'large', type: 'i32' }]
      terms = columns.each_with_index.map do |column, index|
        { kind: :call, name: 'abs', args: [
          { kind: :binary, op: '+', left: { kind: :column, name: column[:name] },
            right: { kind: :param, index: index + 1 } }
        ] }
      end
      node = { kind: :binary, op: '+', left: terms[0], right: terms[1] }
      engine.instance_variable_set(:@native_binding_types, %w[i16 i32])
      engine.instance_variable_set(:@params, [1, 1])
      resolved = engine.compile(node, columns)
      assert(engine.compile(node, columns).equal?(resolved), 'function root did not reuse its resolved identity')
      checks = frame.entry.roots.values.find { |expression, _| expression.equal?(resolved) }&.last
      assert(checks&.chunk(&:itself)&.map(&:first) == [[0, 'i16'], [1, 'i32']],
        "function children lost contextual validation order: #{checks.inspect}")
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
        assert(results.first == results.last, "function contextual replay changed #{params.inspect}: #{results.inspect}")
      end
      assert(frame.depth.zero? && frame.checks.nil?, 'failed function replay retained in-progress checks')

      # Mutate the internal planner syntax, not the public Statement whose
      # routing layer owns a separate AST. Every key must remain an owned copy.
      engine.instance_variable_set(:@native_join_resolution_frame, frame)
      mutable = { kind: :call, name: +'abs', args: [{ kind: :column, name: 'small' }] }
      original = engine.compile(mutable, columns)
      owned = frame.entry.nodes.keys.find { |key| key == mutable }
      mutable[:args][0][:name] = 'large'
      changed = engine.compile(mutable, columns)
      assert(!changed.equal?(original) && original.type == 'i16' && changed.type == 'i32' &&
        owned[:args][0][:name] == 'small' && owned[:name].frozen?,
        'argument mutation changed a retained function or its owned key')
      mutable[:name].replace('gcd')
      mutable[:args] << { kind: :literal, type: 'i32', value: 2 }
      assert(engine.compile(mutable, columns).value == 'gcd', 'changed builtin name reused an old function root')

      # The cold resolved-tree fence also applies to planner-produced roots.
      # Inject only the result of this seam, without manufacturing arbitrary
      # extra metadata that the selected real resolvers cannot produce.
      syntax = { kind: :call, name: 'abs', args: [{ kind: :column, name: 'small' }] }
      leaf = JedNative::Expression.new(kind: :column, type: 'i16', value: 0)
      invalid = [
        JedNative::Expression.new(kind: :host_call, type: 'i16', value: Object.new, args: [leaf]),
        JedNative::Expression.new(kind: :subquery, type: 'i16', value: {}, args: []),
        JedNative::Expression.new(kind: :call, type: 'decimal', value: 'abs', args: [leaf]),
        JedNative::Expression.new(kind: :call, type: 'i16', value: 'random', args: []),
        JedNative::Expression.new(kind: :call, type: 'i16', value: 'abs',
          args: [JedNative::Expression.new(kind: :column, type: 'custom_integer', value: 0)])
      ]
      invalid.each do |expression|
        rejected = fresh_frame
        rejected.physical_observation = { sensitive: false }
        returned = engine.send(:prepared_join_root, rejected, :value, syntax, columns) { expression }
        assert(returned.equal?(expression) && rejected.entry.roots.empty? && !rejected.physical_reuse &&
          rejected.physical_observation[:sensitive], 'unsafe resolved function root entered a root/physical cache')
      end

      # A formerly admitted entry can encounter a new host/stateful syntax
      # shape. Existing unrelated roots may survive; this root and its physical
      # observation must decline publication.
      %w[repeat current_timestamp custom_host].each do |name|
        mutated = { kind: :call, name: name, args: [] }
        count = frame.entry.roots.length
        frame.physical_reuse = true
        frame.physical_observation = { sensitive: false }
        engine.send(:prepared_join_root, frame, :value, mutated, columns) { leaf }
        assert(frame.entry.roots.length == count && !frame.physical_reuse &&
          frame.physical_observation[:sensitive], 'changed unsupported function retained a physical winner')
      end
      engine.instance_variable_get(:@types)['i16'] = {}
      assert(!engine.send(:prepared_join_scalar_tree?, original), 'shadowed scalar type passed resolved function admission')
      engine.instance_variable_get(:@types).delete('i16')
      assert(engine.send(:prepared_join_scalar_tree?, original), 'restored scalar type stayed rejected')
      engine.instance_variable_set(:@native_join_resolution_frame, nil)
      current.close
    end
  end

  def syntax_bounds
    engine = JedNative::Engine.new
    leaf = { kind: :column, name: 'x' }
    call = ->(args) { { kind: :call, name: 'abs', args: args } }
    [191, 192].each do |count|
      assert(engine.send(:prepared_join_nodes?, [call.call(Array.new(count, leaf))]) == (count == 191),
        'function argument nodes escaped the existing 192-node bound')
    end
    [32, 33].each do |depth|
      node = depth.times.reduce(leaf) { |child, _| call.call([child]) }
      assert(engine.send(:prepared_join_nodes?, [node]) == (depth == 32),
        'nested function calls escaped the existing depth bound')
    end
    oversized = { kind: :call, name: 'lower', args: [{ kind: :literal, type: 'text', value: 'x' * 32_769 }] }
    assert(!engine.send(:prepared_join_nodes?, [oversized]), 'function literal escaped the copied-key byte bound')
    ordinary = call.call([leaf])
    [
      { over: {} }, { filter: { kind: :literal, type: 'boolean', value: true } }, { distinct: true },
      { within_group: [] }, { variadic: true }, { order_by: [leaf] }, { arg_names: ['value'] }
    ].each do |modifier|
      assert(!engine.send(:prepared_join_nodes?, [ordinary.merge(modifier)]), 'function modifier entered scalar reuse')
    end
  end

  def scalar_allowance(database)
    sql = "SELECT replace(a.label,'A',$1) #{JOIN} WHERE a.id=1"
    APIS.each do |api|
      answer = compare(database, sql, ['z' * 1024], api: api, warm: ['x'],
        setting: :max_scalar_bytes, limit: 64)
      assert(answer[0] == :error && answer[2] == '54P04',
        'retained replace arguments bypassed the current scalar allowance')
      [1, 256, 65_536].each do |limit|
        compare(database, sql, ['xy'], api: api, warm: ['x'], setting: :max_query_memory_bytes, limit: limit)
      end
    end
  end

  def detached_cache
    JedNative.memory do |database|
      setup(database)
      current = session(database)
      sql = "SELECT lower($1),abs(b.v) #{JOIN} WHERE a.id=1"
      statement, input = current.prepare(sql), +'ORIGINAL'
      2.times { current.query_prepared(statement, input).rows }
      input.replace('CHANGED')
      cursor = current.query_prepared(statement, input)
      execution = cursor.instance_variable_get(:@engine)
      assert(cursor.rows.first.first == 'changed', 'function root captured an earlier bound text value')
      retained = cache(current)
      references = [current, current.engine, cursor, execution, input].map { |object| WeakRef.new(object) }
      current.close
      [retained, references]
    end
  end

  def retention
    retained, references = Thread.new { detached_cache }.value
    assert(retained.values.any? { |entry| entry.roots.any? }, 'function retention fixture did not retain roots')
    3.times { GC.start }
    assert(references.none?(&:weakref_alive?), 'function roots retained an execution owner, cursor or bound value')
  end

  def run
    [false, true].each do |disk|
      Dir.mktmpdir('jed-join-functions-') do |directory|
        database = disk ? JedNative.create(File.join(directory, 'fixture.jed'), locking: :exclusive, work_mem: 1024) : JedNative.memory
        setup(database)
        family_reuse(database)
        error_prefixes(database)
        rejected_calls(database)
        scalar_allowance(database)
        publications_and_snapshots(database)
      ensure
        database&.close
      end
    end
    host_fences
    root_invariants
    syntax_bounds
    retention
    puts 'ruby-native prepared join functions: resolution reuse, builtin guards, costs, mutation and ownership passed'
  end
end

NativePreparedJoinFunctionTests.run
