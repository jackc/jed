# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

# Shared SQL transcripts cover index decisions. These host checks additionally
# preserve resolved-node identity, mutable arguments, and the internal equality
# distinctions which index-expression matching relies on.
module NativeExpressionWalkTests
  APIS = %i[query_prepared execute_prepared query execute].freeze

  class ObservedArray < Array
    attr_reader :flat_maps, :zips

    def flat_map(&block)
      @flat_maps = (@flat_maps || 0) + 1
      super
    end

    def zip(*others)
      @zips = (@zips || 0) + 1
      super
    end
  end

  class OtherExpression < JedNative::Expression; end

  class ArgumentList
    attr_reader :values, :flat_maps, :zips

    def initialize(values) = (@values = values)
    def length = @values.length

    def flat_map(&block)
      @flat_maps = (@flat_maps || 0) + 1
      @values.flat_map(&block)
    end

    def zip(other)
      @zips = (@zips || 0) + 1
      @values.zip(other.is_a?(ArgumentList) ? other.values : other)
    end
  end

  module_function

  def assert(value, message) = (raise message unless value)

  def expression(kind = :literal, type: 'i32', value: 1, args: nil, extra: nil)
    JedNative::Expression.new(kind: kind, type: type, value: value, args: args, extra: extra)
  end

  def engines
    enabled = JedNative::Engine.new
    disabled = JedNative::Engine.new
    disabled.instance_variable_set(:@native_expression_walks_disabled, true)
    [enabled, disabled]
  end

  def ordinary_calls(owner, method)
    count = 0
    trace = TracePoint.new(:call) do |event|
      count += 1 if event.defined_class == owner && event.method_id == method
    end
    result = trace.enable { yield }
    [result, count]
  end

  def helper_outcome(engine, method, *args)
    [:value, engine.send(method, *args)]
  rescue StandardError => error
    [:error, error.class, error.respond_to?(:name) ? error.name : nil]
  end

  def traversal
    enabled, disabled = engines
    first = expression(value: 11)
    second = expression(value: 22)
    hidden = expression(value: 99)
    branch = expression(:call, value: 'coalesce', args: [first, [nil, second, [first]]])
    root = expression(:case, value: hidden, args: [branch, false, { hidden: hidden }, [second, 'x']],
      extra: { filter_expr: hidden, query: hidden })
    expected = [root, branch, first, second, first, second]
    assert(enabled.send(:expression_nodes, root).map(&:object_id) == expected.map(&:object_id),
      'resolved preorder lost node identity, duplicates or nested arrays')
    cases = [nil, false, 12, 'literal', {}, [], [first], [[first, nil], second], root,
      expression(args: false), expression(args: []), expression(args: [nil, false, {}, 'x']),
      [root, root, [first, [branch]]]]
    cases.each do |node|
      actual = enabled.send(:expression_nodes, node)
      expected = disabled.send(:expression_nodes, node)
      assert(actual.map(&:object_id) == expected.map(&:object_id), 'expression traversal differs from original')
      actual.clear
      fresh = enabled.send(:expression_nodes, node)
      assert(!fresh.equal?(actual) && fresh.map(&:object_id) == expected.map(&:object_id),
        'expression traversal retained or reused its result array')
    end
    args = [first]
    mutable = expression(:call, value: 'abs', args: args)
    enabled.send(:expression_nodes, mutable)
    args << [second, first]
    assert(enabled.send(:expression_nodes, mutable).map(&:object_id) == [mutable, first, second, first].map(&:object_id),
      'expression traversal cached mutable argument contents')
    mutable.args = [second]
    assert(enabled.send(:expression_nodes, mutable).map(&:object_id) == [mutable, second].map(&:object_id),
      'expression traversal cached an old argument array')
    actual, new_calls = ordinary_calls(JedNative::QueryPlans, :expression_nodes) { enabled.send(:expression_nodes, root) }
    control, old_calls = ordinary_calls(JedNative::QueryPlans, :expression_nodes) { disabled.send(:expression_nodes, root) }
    assert(actual == control && new_calls.zero? && old_calls.positive?, 'collector control did not select independent original code')
    assert(!enabled.respond_to?(:expression_nodes), 'expression collector changed its private visibility')
  end

  def fallback_shapes
    enabled, disabled = engines
    a, b = expression(value: 1), expression(value: 2)
    subclass = OtherExpression.new(kind: :binary, type: 'boolean', value: 'and', args: [a, b])
    [subclass, [a, subclass]].each do |node|
      actual, calls = ordinary_calls(JedNative::QueryPlans, :expression_nodes) { enabled.send(:expression_nodes, node) }
      expected = disabled.send(:expression_nodes, node)
      assert(actual.map(&:object_id) == expected.map(&:object_id) && calls.positive?,
        'Expression subclass did not retain original traversal fallback')
    end
    subclass_array = ObservedArray.new([a, [b, a]])
    [subclass_array, [a, subclass_array], expression(args: subclass_array)].each do |node|
      before = subclass_array.flat_maps.to_i
      actual = enabled.send(:expression_nodes, node)
      assert(subclass_array.flat_maps.to_i > before, 'Array subclass flat_map fallback was skipped')
      expected = disabled.send(:expression_nodes, node)
      assert(actual.map(&:object_id) == expected.map(&:object_id), 'Array subclass changed expression preorder')
    end
    unusual = ArgumentList.new([a, [b]])
    root = expression(args: unusual)
    actual = enabled.send(:expression_nodes, root)
    assert(unusual.flat_maps.to_i.positive? &&
      actual.map(&:object_id) == disabled.send(:expression_nodes, root).map(&:object_id),
      'non-Array arguments lost original flat_map behavior')

    [ObservedArray.new([a, b]), ArgumentList.new([a, b])].each do |args|
      left, right = expression(args: args), expression(args: [a, b])
      assert(enabled.send(:same_key_expression?, left, right) && args.zips.to_i.positive?,
        'unusual key arguments did not retain original zip fallback')
      assert(disabled.send(:same_key_expression?, left, right), 'key fallback fixture changed')
      conjunction = expression(:binary, type: 'boolean', value: 'and', args: args)
      before = args.flat_maps.to_i
      actual = enabled.send(:conjunctions, conjunction)
      assert(args.flat_maps.to_i > before &&
        actual.map(&:object_id) == disabled.send(:conjunctions, conjunction).map(&:object_id),
        'unusual conjunction arguments did not retain original flat_map behavior')
    end
    actual, calls = ordinary_calls(JedNative::IndexScans, :conjunctions) { enabled.send(:conjunctions, subclass) }
    assert(calls.positive? && actual.map(&:object_id) == [a, b].map(&:object_id),
      'Expression subclass conjunction fallback changed')
  end

  def check_key(enabled, disabled, left, right, expected = nil)
    actual = enabled.send(:same_key_expression?, left, right)
    original = disabled.send(:same_key_expression?, left, right)
    assert(actual == original, "key matcher disagreed: #{left.inspect} / #{right.inspect}")
    assert(actual == expected, 'key matcher violated the explicit equality contract') unless expected.nil?
    actual
  end

  def key_equality
    enabled, disabled = engines
    a = expression(:column, value: 0)
    b = expression(:column, value: 0)
    check_key(enabled, disabled, a, b, true)
    [nil, false, 1, [], {}].each do |other|
      check_key(enabled, disabled, a, other, false)
      check_key(enabled, disabled, other, a, false)
    end
    %i[kind type value].each do |field|
      left, right = a.dup, b.dup
      right[field] = { kind: :param, type: 'i64', value: 1 }.fetch(field)
      check_key(enabled, disabled, left, right, false)
      nan = Float::NAN
      left[field] = right[field] = nan
      check_key(enabled, disabled, left, right, true)
    end
    check_key(enabled, disabled, expression(args: nil), expression(args: []), true)
    check_key(enabled, disabled, expression(args: false), expression(args: []), true)
    check_key(enabled, disabled, expression(args: [a]), expression(args: []), false)
    check_key(enabled, disabled, expression(args: [a]), expression(args: [nil]), false)
    check_key(enabled, disabled, expression(args: [nil]), expression(args: [a]), false)
    nan = Float::NAN
    check_key(enabled, disabled, expression(args: [nan]), expression(args: [nan]), false)
    check_key(enabled, disabled, expression(args: [[nan]]), expression(args: [[nan]]), true)
    # Recursive same-key matching ignores ordinary extra fields. A nested raw
    # Array retains Array/Struct equality, which does compare those fields.
    a.extra, b.extra = { ignored: 1 }, { ignored: 2 }
    check_key(enabled, disabled, expression(args: [a]), expression(args: [b]), true)
    check_key(enabled, disabled, expression(args: [[a]]), expression(args: [[b]]), false)
    a.instance_variable_set(:@native_collation, Object.new)
    b.instance_variable_set(:@native_collation, Object.new)
    check_key(enabled, disabled, a, b, true)
    check_key(enabled, disabled, expression(:collate, type: 'text', value: 'C', args: [a]),
      expression(:collate, type: 'text', value: 'unicode', args: [b]), false)

    left = expression(:cast, type: 'decimal', value: nil, args: [a], extra: { type_mod: [8, 2] })
    right = expression(:cast, type: 'decimal', value: nil, args: [b], extra: { type_mod: [8, 2], ignored: true })
    check_key(enabled, disabled, left, right, true)
    right.extra[:type_mod] = [8, 3]
    check_key(enabled, disabled, left, right, false)
    left.extra = Hash.new([8, 3])
    check_key(enabled, disabled, left, right, true)
    left.extra = Hash.new { |_hash, key| key == :type_mod ? [8, 3] : nil }
    check_key(enabled, disabled, left, right, true)
    left.extra = right.extra = { type_mod: nan }
    check_key(enabled, disabled, left, right, false)
    left.kind = right.kind = :call
    check_key(enabled, disabled, left, right, true)

    wide = expression(:column, type: 'i64', value: 0)
    narrow = expression(:column, type: 'i16', value: 0)
    check_key(enabled, disabled, wide, narrow, false)
    [true, 'i64', 0].each do |marker|
      narrow.extra = { native_body_type: marker }
      check_key(enabled, disabled, wide, narrow, true)
      check_key(enabled, disabled, narrow, wide, true)
    end
    [nil, false].each do |marker|
      narrow.extra = { native_body_type: marker }
      check_key(enabled, disabled, wide, narrow, false)
    end
    narrow.extra = { native_body_type: 'i64' }
    narrow.args, wide.args = [a], []
    check_key(enabled, disabled, wide, narrow, true)
    narrow.type = 'decimal'
    check_key(enabled, disabled, wide, narrow, false)
    narrow.type, narrow.value = 'i16', 1
    check_key(enabled, disabled, wide, narrow, false)

    mutable = expression(:binary, value: '+', args: [expression(value: 1), expression(value: 2)])
    copy = Marshal.load(Marshal.dump(mutable))
    check_key(enabled, disabled, mutable, copy, true)
    mutable.args.last.value = 3
    check_key(enabled, disabled, mutable, copy, false)
    mutable.args = copy.args.dup
    check_key(enabled, disabled, mutable, copy, true)
    _, new_calls = ordinary_calls(JedNative::IndexScans, :same_key_expression?) { enabled.send(:same_key_expression?, mutable, copy) }
    _, old_calls = ordinary_calls(JedNative::IndexScans, :same_key_expression?) { disabled.send(:same_key_expression?, mutable, copy) }
    assert(new_calls.zero? && old_calls.positive?, 'key control did not restore the original comparison')
    assert(!enabled.respond_to?(:same_key_expression?), 'key matcher changed its private visibility')
  end

  def conjunctions
    enabled, disabled = engines
    a, b, c = [1, 2, 3].map { |value| expression(type: 'boolean', value: value) }
    pair = expression(:binary, type: 'boolean', value: 'and', args: [a, b])
    root = expression(:binary, type: 'boolean', value: 'and', args: [pair, c, pair])
    expected = [a, b, c, a, b]
    assert(enabled.send(:conjunctions, root).map(&:object_id) == expected.map(&:object_id),
      'conjunction flattening changed order or duplicates')
    cases = [a, pair, root, expression(:binary, value: 'and', args: []),
      expression(:binary, value: 'or', args: [pair, c]), expression(:call, value: 'and', args: [pair]),
      expression(:binary, value: 'AND', args: [a, b]), expression(:binary, value: :and, args: [a, b])]
    cases.each do |node|
      actual = enabled.send(:conjunctions, node)
      expected = disabled.send(:conjunctions, node)
      assert(actual.map(&:object_id) == expected.map(&:object_id), 'conjunction traversal differs')
      actual.clear
      assert(enabled.send(:conjunctions, node).map(&:object_id) == expected.map(&:object_id), 'conjunction result was reused')
    end
    root.args = [c, pair]
    assert(enabled.send(:conjunctions, root).map(&:object_id) == [c, a, b].map(&:object_id),
      'conjunction flattening retained old arguments')
    [nil, [], expression(:binary, value: 'and'), expression(:binary, value: 'and', args: [a, nil])].each do |node|
      assert(helper_outcome(enabled, :conjunctions, node) == helper_outcome(disabled, :conjunctions, node),
        'conjunction invalid-input error changed')
    end
    _, new_calls = ordinary_calls(JedNative::IndexScans, :conjunctions) { enabled.send(:conjunctions, root) }
    _, old_calls = ordinary_calls(JedNative::IndexScans, :conjunctions) { disabled.send(:conjunctions, root) }
    assert(new_calls.zero? && old_calls.positive?, 'conjunction control did not select the original body')
    assert(!enabled.respond_to?(:conjunctions), 'conjunction helper changed its private visibility')
  end

  def configure(session, disabled)
    session.engine.instance_variable_set(:@native_expression_walks_disabled, disabled)
    session
  end

  def outcome(session, statement, sql, params, api)
    lifetime, phase, rows = session.lifetime_cost, :admission, []
    result = api.to_s.end_with?('_prepared') ? session.public_send(api, statement, *params) : session.public_send(api, sql, *params)
    phase = :pull
    result.is_a?(JedNative::Rows) ? result.each { |row| rows << row.to_a } : rows = result.rows
    [:rows, rows, result.columns, result.column_types, result.cost, session.lifetime_cost - lifetime]
  rescue JedNative::Error => error
    [:error, phase, error.sqlstate, error.message, rows, result&.cost || session.engine.cost, session.lifetime_cost - lifetime]
  ensure
    result&.close
  end

  def compare(database, sql, params = [], api: :query_prepared, setting: nil, limit: 0)
    enabled = configure(database.session, false)
    disabled = configure(database.session, true)
    statements = [enabled, disabled].map { |session| session.prepare(sql) }
    2.times do
      actual = outcome(enabled, statements[0], sql, params, api)
      expected = outcome(disabled, statements[1], sql, params, api)
      assert(actual == expected, "warm expression walks changed #{sql}: #{actual.inspect} != #{expected.inspect}")
    end
    [enabled, disabled].each do |session|
      session.public_send("#{setting}=", setting == :lifetime_max_cost ? session.lifetime_cost + limit : limit) if setting
    end
    actual = outcome(enabled, statements[0], sql, params, api)
    expected = outcome(disabled, statements[1], sql, params, api)
    assert(actual == expected, "expression walks changed #{api}/#{setting}=#{limit}: #{actual.inspect} != #{expected.inspect}")
    actual
  ensure
    enabled&.close
    disabled&.close
  end

  def setup(database)
    database.execute_script(<<~SQL)
      CREATE TABLE ew_t(id i32 PRIMARY KEY, a i32, b i32);
      INSERT INTO ew_t VALUES(1,1,9),(2,2,2),(3,3,7),(4,4,4),(5,5,5),(6,6,1),(7,7,7),(8,8,8),(9,9,9),(10,0,0);
      CREATE INDEX ew_sum ON ew_t((a+b));
      CREATE TABLE ew_p(id i32 PRIMARY KEY, k i32, gone i32);
      INSERT INTO ew_p SELECT g,g%50,CASE WHEN g%20=0 THEN NULL ELSE 1 END FROM generate_series(1,200) AS g;
      CREATE INDEX ew_live ON ew_p(k) WHERE gone IS NULL;
      CREATE INDEX ew_pos ON ew_p(k) WHERE k>10;
    SQL
  end

  def public_queries(database)
    indexed = 'SELECT id FROM ew_t WHERE a+b=10 ORDER BY id'
    queries = [
      [indexed, []],
      ['SELECT id FROM ew_t WHERE b+a=10 ORDER BY id', []],
      ['SELECT id FROM ew_t WHERE a+b=$1 ORDER BY id', [10]],
      ['SELECT id FROM ew_t WHERE a+b=$1 AND a>=$2 AND b>0 ORDER BY id', [10, 3]],
      ['SELECT id FROM ew_t WHERE a+b IN(4,10,14) ORDER BY id', []],
      ['EXPLAIN SELECT id FROM ew_t WHERE a+b=10 ORDER BY id', []],
      ['EXPLAIN SELECT id FROM ew_t WHERE b+a=10 ORDER BY id', []],
      ['SELECT id FROM ew_p WHERE gone IS NULL AND k=$1 ORDER BY id', [20]],
      ['SELECT id FROM ew_p WHERE k>$1 ORDER BY id', [40]],
      ['EXPLAIN SELECT id FROM ew_p WHERE gone IS NULL', []],
      ['EXPLAIN SELECT id FROM ew_p WHERE k>40', []],
      ['EXPLAIN SELECT id FROM ew_p WHERE k>=0', []],
      ['SELECT a,sum(b),count(*) FROM ew_t GROUP BY a HAVING sum(b)>0 ORDER BY a', []],
      ['SELECT id,row_number() OVER(ORDER BY a) FROM ew_t ORDER BY id', []],
      ['SELECT id,CASE WHEN a>0 THEN b ELSE NULL END FROM ew_t ORDER BY id', []],
      ['SELECT id,10/(a-$1) FROM ew_t WHERE id<=3 ORDER BY id', [3]]
    ]
    APIS.each do |api|
      queries.each { |sql, params| compare(database, sql, params, api: api) }
      result = compare(database, indexed, api: api)
      # Existing ddl/expression_index.test is the authority for this fixture,
      # including the exact16 cost after the expression index is added.
      assert(result[0, 5] == [:rows, [[1], [3], [5]], ['id'], ['i32'], 16],
        'expression-index reference values, metadata or cost changed')
      explain = compare(database, 'EXPLAIN SELECT id FROM ew_p WHERE gone IS NULL', api: api)
      assert(explain[1] == [[0, 'Filter', 'conjuncts=1', 1, 5],
        [1, 'Scan ew_p', 'Index scan: using ew_live; touched=2', 1, 3]],
        'partial-index EXPLAIN differs from shared index_partial_explain fixture')
      range = compare(database, 'EXPLAIN SELECT id FROM ew_p WHERE k>40', api: api)
      assert(range[1] == [[0, 'Filter', 'conjuncts=1', 67, 269],
        [1, 'Scan ew_p', 'Index bound: using ew_pos; touched=2', 67, 135]],
        'partial-index implication EXPLAIN changed')
      failure = compare(database, 'SELECT id,10/(a-$1) FROM ew_t WHERE id<=3 ORDER BY id', [3], api: api)
      assert(failure[0] == :error && failure[2] == '22012', 'division error fixture did not fail')
      (1..17).each do |limit|
        %i[max_cost lifetime_max_cost].each do |setting|
          compare(database, indexed, api: api, setting: setting, limit: limit)
        end
      end
      (1..8).each do |limit|
        compare(database, 'SELECT id,10/(a-$1) FROM ew_t WHERE id<=3 ORDER BY id', [3],
          api: api, setting: :max_cost, limit: limit)
      end
    end
  end

  def run
    traversal
    key_equality
    conjunctions
    fallback_shapes
    JedNative.memory { |database| setup(database); public_queries(database) }
    Dir.mktmpdir('jed-ruby-expression-walk') do |directory|
      JedNative.create(File.join(directory, 'fixture.jed'), locking: :exclusive) do |database|
        setup(database)
        public_queries(database)
      end
    end
    puts 'ruby-native expression walks: preorder identity, key equality, mutable arguments, index decisions and exact cost prefixes passed'
  end
end

NativeExpressionWalkTests.run
