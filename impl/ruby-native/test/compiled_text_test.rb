# frozen_string_literal: true

require 'tmpdir'
require 'weakref'
require_relative '../lib/jed_native'

# Shared corpus tests own SQL semantics. These internal probes compare the new
# text templates with the independent evaluator, including metadata, every
# small accounting prefix, deferred reads, and reuse of captured host contexts.
module NativeCompiledTextTests
  module_function

  COLUMNS = [{ name: 'a', type: 'text' }, { name: 'b', type: 'text' }].freeze

  def assert(value, message) = (raise(message) unless value)

  def engine(params = [], types = [])
    current = JedNative::Engine.new
    current.instance_variable_set(:@params, params)
    current.instance_variable_set(:@native_binding_types, types)
    current.instance_variable_set(:@native_compilation_disabled, true)
    current
  end

  def resolve(sql, types: [], columns: [], filter: false, environment: nil, default: 'C')
    current = engine(Array.new(types.length), types)
    current.instance_variable_set(:@environment, environment) if environment
    current.default_collation = default
    ast = JedNative::Statement.new(sql).ast
    expression = filter ? current.compile_filter(ast[:where], columns) : current.compile(ast[:columns].first[:expr], columns)
    [expression, current.instance_variable_get(:@types)]
  end

  def metadata(value)
    case value
    when Array then value.map { |item| metadata(item) }
    when JedNative::CollatedText
      label = value.collation_label
      [value.class.name, value.to_s, label.to_h.transform_values { |item| item.is_a?(JedNative::CollationLabel) ? item.to_h : item }]
    when String then [value.class.name, value.to_s]
    else value
    end
  end

  class ProbeRow
    def initialize(values, events, failure)
      @values, @events, @failure = values, events, failure
    end
    def [](index)
      @events << [:read, index]
      raise JedNative::Error.new('XX001', 'deferred text column failed') if index == @failure
      @values[index]
    end
  end

  class HostText < String
    def length = super + 3
    def <=>(other) = -(super)
  end

  module AccountProbe
    attr_accessor :text_events
    def charge(amount = 1)
      text_events << [:charge, amount, cost]
      super
    end
  end

  def direct_outcome(expression, kernel, params, types, values, failure, statement, lifetime, prior)
    current = engine(params, types)
    current.singleton_class.prepend(AccountProbe)
    events = current.text_events = []
    current.max_cost, current.lifetime_max_cost = statement, lifetime
    current.instance_variable_set(:@lifetime_cost, prior)
    row = ProbeRow.new(values, events, failure)
    begin
      value = kernel ? kernel.call(current, row, params) : current.evaluate(expression, row)
      result = [:value, metadata(value)]
    rescue JedNative::Error => error
      result = [:error, error.sqlstate, error.message]
    end
    [*result, current.cost, current.lifetime_cost, events]
  end

  def direct_compare(sql, params: [], types: [], columns: [], values: [], failure: nil, filter: false, expression: nil, expected: nil)
    expression, registry = expression ? [expression, {}] : resolve(sql, types: types, columns: columns, filter: filter)
    kernel = JedNative::CompiledExpression.build(expression, types: registry)
    assert(kernel, "supported text expression rejected: #{sql}")
    arguments = [params, types, values, failure]
    plain = direct_outcome(expression, nil, *arguments, 0, 0, 0)
    actual = direct_outcome(expression, kernel, *arguments, 0, 0, 0)
    assert(actual == plain, "text expression changed #{sql}: #{actual.inspect} != #{plain.inspect}")
    assert(plain[0, expected.length] == expected, "unexpected reference result for #{sql}: #{plain.inspect}") if expected
    (1..plain[-3] + 1).each do |limit|
      [[limit, 0, 0], [0, 7 + limit, 7], [limit, 7 + limit, 7]].each do |statement, lifetime, prior|
        baseline = direct_outcome(expression, nil, *arguments, statement, lifetime, prior)
        result = direct_outcome(expression, kernel, *arguments, statement, lifetime, prior)
        assert(result == baseline, "text guard changed #{sql} at #{[statement, lifetime, prior].inspect}: #{result.inspect} != #{baseline.inspect}")
      end
    end
    assert(kernel.source_bytes <= JedNative::CompiledExpression::MAX_SOURCE_BYTES && kernel.pool.frozen?, 'text kernel escaped source/pool limits')
    kernel
  end

  def direct_expressions
    pairs = [[nil, nil], [nil, 'long'], ['long', nil], ['', ''], ['', 'a'], ['abc', 'abc'],
             ['abc', 'abd'], ['prefix', 'prefix-long'], ['é🙂', 'ê🙂'], ["a\u0000b", "a\u0000c"], ['é', 'é']]
    %w[= <> != < <= > >=].each do |operator|
      pairs.each do |values|
        direct_compare("SELECT a #{operator} b", columns: COLUMNS, values: values)
        direct_compare("SELECT $1 #{operator} $2", types: %w[text text], params: values)
      end
    end
    ["SELECT 'é🙂'", 'SELECT NULL::text', 'SELECT a', 'SELECT a::text', 'SELECT a IS NULL', 'SELECT a IS NOT NULL'].each do |sql|
      [nil, '', 'é🙂'].each { |value| direct_compare(sql, columns: COLUMNS, values: [value]) }
    end
    [nil, '', 'é🙂'].each do |value|
      direct_compare('SELECT $1::text', types: ['text'], params: [value])
      expression = JedNative::Expression.new(kind: :param, type: 'text', value: 0, extra: { context_type: true })
      direct_compare('contextual text parameter', params: [value], types: ['text'], expression: expression)
    end
    ['a < b', 'a = b', 'a <> b'].each do |predicate|
      direct_compare("SELECT #{predicate}", columns: COLUMNS, values: [nil, 'x'], failure: 1, expected: [:error, 'XX001'])
    end
    direct_compare("SELECT a = 'stop' AND b > 'x'", columns: COLUMNS, values: ['go', 'y'], failure: 1, expected: [:value, false])
    direct_compare("SELECT a = 'stop' OR b > 'x'", columns: COLUMNS, values: ['stop', 'y'], failure: 1, expected: [:value, true])
    direct_compare("SELECT a = 'stop' AND b > 'x'", columns: COLUMNS, values: [nil, 'y'], failure: 1, expected: [:error, 'XX001'])
    direct_compare("SELECT 1 WHERE a = 'stop' AND b > 'x'", columns: COLUMNS, values: [nil, 'y'], failure: 1, filter: true, expected: [:value, nil])
    direct_compare("SELECT a < 'zz' AND 1 / 0 > 0", columns: COLUMNS, values: ['a'], expected: [:error, '22012'])
    direct_compare("SELECT CASE WHEN a < 'zz' THEN 7 ELSE 1 / 0 END", columns: COLUMNS, values: ['a'], expected: [:value, 7])
  end

  def unicode_environment
    @unicode_environment ||= JedNative::Environment.new.load_unicode_data(File.binread(File.expand_path('../../../spec/collation/fixtures/unicode.jucd', __dir__)))
  end

  def collation_boundaries
    expression, = resolve('SELECT $1 < $2', types: %w[text text])
    labels = [JedNative::CollationLabel.new(name: 'C').freeze,
      JedNative::CollationLabel.new(name: 'unicode', table: unicode_environment.collations.fetch('unicode')).freeze,
      JedNative::CollationLabel.new(conflict: true).freeze, JedNative::CollationLabel.new(none: true).freeze]
    labels.each do |label|
      params = [JedNative::CollatedText.new('z', label), JedNative::CollatedText.new('ä', label)]
      direct_compare('dynamic text labels', expression: expression, params: params, types: %w[text text])
      equality, = resolve('SELECT $1 = $2', types: %w[text text])
      direct_compare('dynamic equality labels', expression: equality, params: params, types: %w[text text])
      direct_compare('dynamic null label', expression: expression, params: [nil, params.last], types: %w[text text])
    end
    [nil, false, true].product([nil, false, true], [nil, false], [nil, false]).each do |explicit, default, conflict, none|
      label = JedNative::CollationLabel.new(name: 'C', explicit: explicit, default: default, conflict: conflict, none: none)
      expression = JedNative::Expression.new(kind: :column, type: 'text', value: 0)
      expression.instance_variable_set(:@native_collation, label)
      kernel = direct_compare('C label metadata', expression: expression, values: ['unchanged'])
      label.name = 'mutated'
      result = kernel.call(engine, ['unchanged'], [])
      assert(result.collation_label.name == 'C' && result.collation_label.frozen?, 'text kernel retained mutable resolution label')
      assert(kernel.pool.none? { |item| item.equal?(label) }, 'text pool retained external label')
    end
    ["SELECT 'a' COLLATE \"C\"", "SELECT 'a' COLLATE unicode", "SELECT upper('a')", "SELECT 'a' || 'b'",
     "SELECT CASE WHEN TRUE THEN 'a' ELSE 'b' END", "SELECT coalesce(NULL::text, 'a')", "SELECT 'a' LIKE 'a_b'"].each do |sql|
      node, registry = resolve(sql, environment: unicode_environment)
      assert(JedNative::CompiledExpression.build(node, types: registry).nil?, "unsupported text shape compiled: #{sql}")
    end
    ['unicode', 'es'].each do |name|
      node, registry = resolve("SELECT 'z' < 'ä'", environment: unicode_environment, default: name)
      assert(JedNative::CompiledExpression.build(node, types: registry).nil?, 'loaded default collation compiled as C')
    end
    labels.drop(1).each do |label|
      node = JedNative::Expression.new(kind: :column, type: 'text', value: 0)
      node.instance_variable_set(:@native_collation, label)
      assert(JedNative::CompiledExpression.build(node, types: {}).nil?, 'unsafe static text label entered compiler')
    end
    mixed = [JedNative::CollatedText.new('z', labels[0]), JedNative::CollatedText.new('ä', labels[1])]
    direct_compare('dynamic collation conflict', expression: resolve('SELECT $1 < $2', types: %w[text text]).first,
      params: mixed, types: %w[text text], expected: [:error, '42P22'])
    # A statically C comparison can still have an unlabeled parameter carrying
    # its own runtime label. Its comparison and work follow the existing helper.
    %w[= <> < <= > >=].each do |operator|
      [JedNative::CollatedText.new('z', labels[1]), HostText.new('z'), JedNative::Bytea.new('é🙂')].each do |value|
        direct_compare("SELECT $1 #{operator} 'ä'", params: [value], types: ['text'])
      end
    end
  end

  def source_and_admission
    marker = "x\"; raise 'text source escaped'; #\n#{'é' * 40}"
    expression = JedNative::Expression.new(kind: :literal, type: 'text', value: marker.dup)
    kernel = direct_compare('trusted literal', expression: expression)
    assert(!kernel.source.include?(marker) && kernel.pool.include?(marker), 'text literal escaped its data pool')
    expression.value.replace('changed')
    assert(kernel.call(engine, [], []) == marker, 'literal pool aliases its parser input')
    assert(kernel.pool.grep(String).all?(&:frozen?), 'text literal pool is mutable')
    literal = JedNative::Expression.new(kind: :literal, type: 'text', value: JedNative::CollatedText.new('x', JedNative::CollationLabel.new(name: 'C')))
    assert(JedNative::CompiledExpression.build(literal, types: {}).nil?, 'String subclass literal crossed immutable pool boundary')
    node, = resolve("SELECT a >= 'z'", columns: COLUMNS)
    assert(JedNative::CompiledExpression.build(node, types: {}, text: false).nil?, 'text opt-out admitted comparison')
    assert(JedNative::CompiledExpression.build(node, types: { 'text' => [] }).nil?, 'nominal text type shadow was ignored')
    assert(JedNative::CompiledRelation.prepare_filter(node, text: false).nil?, 'relation ignored text opt-out')
    assert(JedNative::CompiledExpression.prepare_many([node], types: {}, text: false).nil?, 'argument compiler ignored text opt-out')
    assert(JedNative::CompiledPipeline.build(node, [node], types: {}, mode: :array, text: false).nil?, 'pipeline ignored text opt-out')
    literal = JedNative::Expression.new(kind: :literal, type: 'text', value: 'x')
    assert(JedNative::CompiledOrdering.build([literal], [{}]).nil?, 'text comparator exceeded this change scope')
    %w[min max count].each do |name|
      aggregate = JedNative::Expression.new(kind: :call, type: name == 'count' ? 'i64' : 'text', value: name, args: [literal], extra: {})
      assert(JedNative::CompiledAggregate.prepare(aggregate).nil?, 'text aggregate reduction exceeded this change scope')
    end
  end

  def pool_byte_bounds
    maximum = JedNative::CompiledExpression::MAX_POOL_BYTES
    literal = ->(value) { JedNative::Expression.new(kind: :literal, type: 'text', value: value) }
    # The cap measures retained UTF-8 bytes, not the character work charged by
    # SQL comparison. Four-byte characters make those two quantities distinct.
    exact = '🙂' * (maximum / 4)
    assert(exact.bytesize == maximum && exact.length < maximum, 'pool byte-bound fixture is invalid')
    kernel = JedNative::CompiledExpression.build(literal.call(exact), types: {})
    assert(kernel && kernel.pool.grep(String).sum(&:bytesize) == maximum, 'exact text pool byte cap was rejected')
    oversized = exact + 'x'
    def oversized.dup = raise('oversized text pool value was copied before rejection')
    assert(JedNative::CompiledExpression.build(literal.call(oversized), types: {}).nil?, 'text pool admitted cap plus one byte')

    half = 'é' * (maximum / 4)
    program = JedNative::CompiledRelation.prepare_keys([literal.call(half), literal.call(half)])
    assert(program && program.pool.grep(String).sum(&:bytesize) == maximum, 'cumulative exact text pool byte cap was rejected')
    assert(JedNative::CompiledRelation.prepare_keys([literal.call(half), literal.call(half + 'x')]).nil?, 'text pool capped each value instead of cumulative bytes')
    comparison = ->(left, right) { JedNative::Expression.new(kind: :binary, type: 'boolean', value: '=', args: [left, right]) }
    assert(JedNative::CompiledExpression.build(comparison.call(literal.call(exact), literal.call('')), types: {}).nil?,
      'generated operator strings were omitted from pool byte accounting')

    unsupported = JedNative::Expression.new(kind: :call, type: 'text', value: 'upper', args: [literal.call('x')], extra: {})
    rejected = comparison.call(literal.call(exact), unsupported)
    supported = comparison.call(literal.call('a'), literal.call('b'))
    mixed = JedNative::CompiledExpression.build_many([rejected, supported], types: {})
    assert(mixed && mixed.pool.first.equal?(rejected), 'rejected text root prevented mixed projection fallback')
    assert(mixed.pool.grep(String).sum(&:bytesize) == 3, 'rejected root did not roll back copied pool bytes')
    assert(mixed.call(engine, [], []) == [false, false], 'pool-byte rollback changed fallback projection results')
  end

  def setup(database)
    database.execute('CREATE TABLE text_t(id i32 PRIMARY KEY, label text, other text)')
    database.execute("INSERT INTO text_t VALUES(1,'alpha','alpha'),(2,NULL,'beta'),(3,'é🙂','ê🙂'),(4,'',''),(5,'z','ä'),(6,'stop',NULL)")
    database.execute('CREATE TABLE text_limit(value text)')
    database.execute("INSERT INTO text_limit VALUES('a')")
  end

  def session(database, disabled: false, global: false)
    current = database.session
    current.engine.instance_variable_set(:@native_text_compilation_disabled, disabled)
    current.engine.instance_variable_set(:@native_compilation_disabled, global)
    current
  end

  def invoke(current, prepared, sql, params, api)
    case api
    when :execute then current.execute(sql, *params)
    when :query then current.query(sql, *params)
    when :execute_prepared then current.execute_prepared(prepared, *params)
    when :query_prepared then current.query_prepared(prepared, *params)
    end
  end

  def outcome(current, prepared, sql, params, api, collect: false, take: nil)
    prior = current.lifetime_cost
    phase, rows, checkpoints = :admission, [], []
    result = invoke(current, prepared, sql, params, api)
    phase = :pull
    if result.is_a?(JedNative::Rows) && !collect
      result.each do |row|
        rows << row.to_a
        checkpoints << [result.cost, current.lifetime_cost - prior]
        break if take && rows.length >= take
      end
    else
      rows = result.rows
    end
    [:rows, rows, result.columns, result.column_types, checkpoints, result.cost, current.lifetime_cost - prior]
  rescue JedNative::Error => error
    [:error, phase, error.sqlstate, error.message, rows, checkpoints, result&.cost || current.engine.cost, current.lifetime_cost - prior]
  ensure
    result.close if result.is_a?(JedNative::Rows)
  end

  def compare(database, sql, params = [], api: :query_prepared, setting: nil, limit: 0, collect: false, take: nil, global: false)
    results = [true, false].map do |disabled|
      current = session(database, disabled: disabled, global: global && disabled)
      prepared = current.prepare(sql)
      3.times { outcome(current, prepared, sql, params, api) }
      if setting == :lifetime_max_cost
        current.lifetime_max_cost = current.lifetime_cost + limit
      elsif setting
        current.public_send("#{setting}=", limit)
      end
      outcome(current, prepared, sql, params, api, collect: collect, take: take)
    ensure
      current&.close
    end
    assert(results[0] == results[1], "text compilation changed #{api} #{sql} #{params.inspect} #{setting}=#{limit}: #{results.inspect}")
    results.last
  end

  def dispatched
    kernels = []
    trace = TracePoint.new(:call) do |event|
      if event.method_id == :call && event.self.is_a?(JedNative::CompiledExpression::Kernel) && event.self.source.include?('# C text specialization')
        kernels << event.self
      end
    end
    trace.enable { yield }
    kernels
  ensure
    trace&.disable
  end

  def api_equivalence(database)
    cases = [
      ['SELECT label, label >= $1 FROM text_t WHERE label <> $2 ORDER BY id', ['a', 'skip']],
      ['SELECT label::text, other < label FROM text_t WHERE label >= $1 LIMIT 2 OFFSET 1', ['']],
      ['SELECT sum(id) FROM text_t WHERE label >= $1', ['a']],
      ['SELECT count(*) FILTER (WHERE label < $1), sum(id) FILTER (WHERE label <> other) FROM text_t', ['z']],
      ['SELECT label, count(*) FROM text_t GROUP BY label ORDER BY label', []],
      ['SELECT label, count(*) FROM text_t GROUP BY ROLLUP(label) HAVING label IS NULL OR label >= $1 ORDER BY label', ['a']],
      ['SELECT label >= $1 AND 10 / (id - 5) > 0 FROM text_t ORDER BY id', ['']],
      ['SELECT id FROM text_t WHERE label < $1 AND 10 / (id - 3) > 0 ORDER BY id', [nil]],
      ['SELECT id FROM text_t WHERE label < $1 OR 10 / (id - 3) > 0 ORDER BY id', [nil]],
      ["SELECT label COLLATE \"C\", upper(label), label LIKE 'a%' FROM text_t ORDER BY id", []]
    ]
    %i[execute query execute_prepared query_prepared].each do |api|
      cases.each { |sql, params| compare(database, sql, params, api: api, global: true) }
      compare(database, cases[0][0], [nil, 'skip'], api: api)
      compare(database, cases[0][0], [7, 'skip'], api: api)
      compare(database, cases[0][0], ['a'], api: api)
    end
    %i[execute_prepared query_prepared].each do |api|
      [cases[0], cases[2], cases[6]].each do |sql, params|
        complete = compare(database, sql, params, api: api)
        %i[max_cost lifetime_max_cost].each do |setting|
          (1..complete[-2] + 1).each { |limit| compare(database, sql, params, api: api, setting: setting, limit: limit) }
        end
        [1, 127, 512, 2048, 1_048_576].each do |limit|
          compare(database, sql, params, api: api, setting: :max_query_memory_bytes, limit: limit, collect: true)
        end
      end
      compare(database, cases[6][0], cases[6][1], api: api, take: 1)
    end
  end

  def invalidation_and_control(database)
    current = session(database)
    sql = 'SELECT label, label >= $1 FROM text_t WHERE label <> $2'
    prepared = current.prepare(sql)
    3.times { outcome(current, prepared, sql, ['a', 'skip'], :query_prepared) }
    kernels = dispatched { outcome(current, prepared, sql, ['a', 'skip'], :query_prepared) }.uniq
    assert(!kernels.empty?, 'text query did not dispatch a generated kernel')
    current.engine.instance_variable_set(:@native_text_compilation_disabled, true)
    assert(dispatched { outcome(current, prepared, sql, ['a', 'skip'], :query_prepared) }.empty?, 'text opt-out reused warmed text kernels')
    current.engine.instance_variable_set(:@native_text_compilation_disabled, false)
    assert(dispatched { outcome(current, prepared, sql, ['a', 'skip'], :query_prepared) }.uniq == kernels, 'text opt-in lost cached program')
    current.engine.instance_variable_set(:@native_compilation_disabled, true)
    assert(dispatched { outcome(current, prepared, sql, ['a', 'skip'], :query_prepared) }.empty?, 'global opt-out reused warmed text kernels')
    current.engine.instance_variable_set(:@native_compilation_disabled, false)
    input = +'a'
    captured = current.query_prepared(prepared, input, 'skip')
    input.replace('zzzz')
    expected = compare(database, sql, ['a', 'skip'])[1]
    assert(captured.rows == expected, 'text cursor aliased mutable host bindings')
    first = current.query_prepared(prepared, 'a', 'skip').rows
    first.first.first.replace('mutated')
    assert(current.query_prepared(prepared, 'a', 'skip').rows == expected, 'text result aliases a cached value')
    current.revoke(:select, 'text_t')
    denied = outcome(current, prepared, sql, ['a', 'skip'], :query_prepared)
    assert(denied[0] == :error && denied[2] == '42501', 'compiled text query bypassed revoked read permission')
    current.close

    reader, writer = session(database), session(database)
    sql = 'SELECT sum(id) FROM text_t WHERE label >= (SELECT value FROM text_limit)'
    prepared = reader.prepare(sql)
    3.times { reader.query_prepared(prepared).rows }
    old = reader.query_prepared(prepared)
    writer.execute("UPDATE text_limit SET value = 'z'")
    assert(old.rows == [[15]] && reader.query_prepared(prepared).rows == [[8]], 'compiled text relation retained folded literal or lost snapshot')
    sql = 'SELECT label >= $1 FROM text_t ORDER BY id'
    prepared = reader.prepare(sql)
    3.times { reader.query_prepared(prepared, 'a').rows }
    old = reader.query_prepared(prepared, 'a')
    writer.execute('DROP TABLE text_t')
    writer.execute('CREATE TABLE text_t(id i16 PRIMARY KEY, label text)')
    writer.execute("INSERT INTO text_t VALUES(1,'replacement')")
    assert(old.rows.length == 6 && reader.query_prepared(prepared, 'z').rows == [[false]], 'text plan retained replaced table or lost old schema')
    reader.close
    writer.close
  end

  def default_collation_invalidation
    JedNative.memory(environment: unicode_environment) do |database|
      current = session(database)
      sql = "SELECT $1::text < 'ä'"
      prepared = current.prepare(sql)
      3.times { current.query_prepared(prepared, 'z').rows }
      before = current.query_prepared(prepared, 'z')
      database.engine.default_collation = 'unicode'
      reference = session(database, disabled: true)
      expected = outcome(reference, reference.prepare(sql), sql, ['z'], :query_prepared)
      assert(dispatched { assert(outcome(current, prepared, sql, ['z'], :query_prepared) == expected, 'default collation change reused C program') }.empty?, 'non-C default compiled a text program')
      assert(before.rows == [[true]], 'unpulled text cursor adopted a later default collation')
      database.engine.default_collation = 'C'
      assert(current.query_prepared(prepared, 'z').rows == [[true]], 'returning to C retained loaded collation ordering')
      ["SELECT 'a' COLLATE \"C\" < 'b' COLLATE unicode", "SELECT a < b FROM (SELECT 'a' COLLATE \"C\" AS a, 'b' COLLATE unicode AS b) t"].each do |conflict|
        compare(database, conflict)
      end
      current.close
      reference.close
    end
  end

  def detached_kernel
    current = engine(['parameter'], ['text'])
    expression = current.compile(JedNative::Statement.new("SELECT a >= $1").ast[:columns].first[:expr], COLUMNS)
    row, params = ['value'], ['parameter']
    kernel = JedNative::CompiledExpression.build(expression, types: {})
    kernel.call(current, row, params)
    [kernel, WeakRef.new(current), WeakRef.new(row), WeakRef.new(params), WeakRef.new(expression)]
  end

  def cache_and_lifetime
    # A completed thread drops conservative VM stack roots; the surviving
    # kernel must retain none of its input or resolution objects.
    kernel, *references = Thread.new { detached_kernel }.value
    3.times { GC.start }
    assert(references.none?(&:weakref_alive?), 'text kernel captured engine, rows, parameters or expression')
    assert(kernel.call(engine, ['z'], ['a']) == true, 'detached text kernel requires discarded context')

    current = engine
    current.instance_variable_set(:@native_compilation_disabled, false)
    current.instance_variable_set(:@native_compilation_immediate, true)
    predicate, = resolve("SELECT a >= 'first'", columns: COLUMNS)
    keys = [resolve('SELECT a', columns: COLUMNS).first]
    aggregate, = resolve("SELECT count(*) FILTER (WHERE a >= 'first')", columns: COLUMNS)
    arguments, = resolve("SELECT count(a >= 'first')", columns: COLUMNS)
    [[:native_compiled_relation_filter, predicate], [:native_compiled_relation_keys, keys],
     [:native_compiled_aggregate, aggregate], [:native_compiled_aggregate_arguments, arguments]].each do |method, node|
      original = current.public_send(method, node)
      assert(original, "text helper did not compile: #{method}")
      current.instance_variable_set(:@native_text_compilation_disabled, true)
      assert(current.public_send(method, node).nil?, "text helper ignored disable after memoization: #{method}")
      current.instance_variable_set(:@native_text_compilation_disabled, false)
      assert(current.public_send(method, node).equal?(original), "text helper rebuilt original memo: #{method}")
    end
    70.times do |index|
      node, = resolve("SELECT a >= 'literal#{index}'", columns: COLUMNS)
      current.native_compiled_relation_filter(node)
    end
    cache = current.instance_variable_get(:@native_compiled_relations)
    assert(cache.length == JedNative::CompiledPlans::MAX_RELATION_CACHE_ENTRIES && !cache.compare_by_identity?, 'text relation cache lost its source-and-pool bound')
    assert(cache.keys.map(&:last).uniq.length == cache.length, 'text literal pools were collapsed into one cache entry')
    assert(cache.values.none? { |entry| entry.pool.any? { |item| item.is_a?(JedNative::Expression) } }, 'text cache retained resolved expressions')
  end

  def host_fallback
    calls = []
    environment = JedNative::Environment.new.register_function(name: 'text_probe', args: ['text'], returns: 'text', cost: 3) do |value|
      calls << value
      raise JedNative::Error.new('22023', 'text callback failed') if value == 'stop'
      value
    end
    JedNative.memory(environment: environment) do |database|
      setup(database)
      sql = "SELECT label >= 'a', text_probe(label), other < 'z' FROM text_t ORDER BY id"
      %i[execute query execute_prepared query_prepared].each do |api|
        results = [true, false].map do |disabled|
          current = session(database, disabled: disabled)
          prepared = current.prepare(sql)
          3.times { outcome(current, prepared, sql, [], api) }
          calls.clear
          result = outcome(current, prepared, sql, [], api)
          [result, calls.dup]
        ensure
          current&.close
        end
        assert(results[0] == results[1] && results.last.first[2] == '22023', 'mixed text compiler changed callback error/order')
      end
      current = session(database)
      prepared = current.prepare(sql)
      3.times { outcome(current, prepared, sql, [], :query_prepared) }
      current.revoke(:execute, 'text_probe')
      calls.clear
      denied = outcome(current, prepared, sql, [], :query_prepared)
      assert(denied[0] == :error && denied[2] == '42501' && calls.empty?, 'text kernel bypassed callback permission')
      current.close
    end
  end

  def run
    direct_expressions
    collation_boundaries
    source_and_admission
    pool_byte_bounds
    cache_and_lifetime
    JedNative.memory do |database|
      setup(database)
      api_equivalence(database)
      invalidation_and_control(database)
    end
    Dir.mktmpdir('jed-compiled-text-') do |directory|
      JedNative.create(File.join(directory, 'text.jed'), locking: :exclusive) do |database|
        setup(database)
        api_equivalence(database)
      end
    end
    default_collation_invalidation
    host_fallback
    puts 'ruby-native compiled text: metadata, character work, deferred reads, exact guards, collation fallback, warm APIs and captured contexts passed'
  end
end

NativeCompiledTextTests.run
