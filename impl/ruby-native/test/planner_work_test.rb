# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

# The shared corpus owns estimates and physical choices. These probes exercise
# the resolved-tree helper's mutable inputs and the context-sensitive overrides
# which a scalar fast path must leave in their original evaluation order.
module NativePlannerWorkTests
  APIS = %i[query_prepared execute_prepared query execute].freeze
  LEAVES = %i[literal column param outer source star].freeze
  INTERIORS = %i[unary binary cast numeric_context].freeze

  class OtherExpression < JedNative::Expression; end

  class ObservedArray < Array
    attr_reader :sums
    def sum(&block)
      @sums = (@sums || 0) + 1
      super(&block)
    end
  end

  class Arguments
    attr_reader :sums
    def initialize(values) = (@values = values)
    def sum(&block)
      @sums = (@sums || 0) + 1
      @values.sum(&block)
    end
  end

  class ObservedHash < Hash
    def initialize(events, values)
      @events = events
      super()
      merge!(values)
    end
    def [](key)
      @events << key
      super
    end
  end

  class CostProbe
    attr_accessor :amount
    def initialize(events, name, amount, failure: false)
      @events, @name, @amount, @failure = events, name, amount, failure
    end
    def cost
      @events << @name
      raise JedNative::Error.new('22000', @name.to_s) if @failure
      @amount
    end
  end

  module_function

  def assert(value, message) = (raise message unless value)
  def expression(kind = :literal, value: 1, args: nil, extra: nil, type: 'i32')
    JedNative::Expression.new(kind: kind, type: type, value: value, args: args, extra: extra)
  end

  def engines
    [JedNative::Engine.new, JedNative::Engine.new].tap do |pair|
      pair.last.instance_variable_set(:@native_planner_work_disabled, true)
    end
  end

  def work(engine, node)
    before = [engine.cost, engine.lifetime_cost]
    result = engine.send(:expression_work, node)
    assert([engine.cost, engine.lifetime_cost] == before, 'planner work charged the runtime meter')
    result
  end

  def helper_outcome(engine, node)
    [:value, work(engine, node)]
  rescue StandardError => error
    [:error, error.class, error.respond_to?(:sqlstate) ? error.sqlstate : nil,
      error.respond_to?(:name) ? error.name : nil, error.is_a?(JedNative::Error) ? error.message : nil]
  end

  def original_calls
    calls, fibers = Hash.new(0), {}
    trace = TracePoint.new(:call) do |event|
      next unless event.method_id == :expression_work
      calls[event.defined_class] += 1
      fibers[Fiber.current.object_id] = true
    end
    value = trace.enable { yield }
    [value, calls, fibers.length]
  end

  def scalar_shapes
    enabled, disabled = engines
    leaf = expression
    branch = expression(:binary, value: '+', args: [leaf, leaf])
    root = expression(:binary, value: '+', args: [branch, branch, [branch, nil, false, {}, 'ignored']])
    assert(work(enabled, root) == 4, 'work estimate lost duplicate occurrences or nested argument arrays')
    (LEAVES + INTERIORS).each do |kind|
      [nil, [], [leaf], [branch, [branch, leaf]]].each do |args|
        node = expression(kind, args: args)
        expected = (LEAVES.include?(kind) ? 0 : 1) + (args && !args.empty? && args.first.equal?(branch) ? 2 : 0)
        assert(work(enabled, node) == expected && work(disabled, node) == expected,
          "scalar work changed for #{kind}/#{args.inspect}")
      end
    end
    [nil, false, 0, 'literal', {}, [], [root, root, [branch]], Object.new].each do |node|
      assert(helper_outcome(enabled, node) == helper_outcome(disabled, node), 'non-expression/root-array work changed')
    end
    actual, new_calls, = original_calls { work(enabled, root) }
    expected, old_calls, = original_calls { work(disabled, root) }
    assert(actual == expected && new_calls[JedNative::QueryPlans] < old_calls[JedNative::QueryPlans] &&
      old_calls[JedNative::HostFunctions].positive?, 'disable switch did not restore independent original work traversal')
    mutable = expression(:literal, args: [branch])
    assert(work(enabled, mutable) == 1, 'zero-own-work node skipped its arguments')
    mutable.args << branch
    assert(work(enabled, mutable) == 2, 'work estimate cached an argument array')
    mutable.args = [leaf]
    mutable.kind = :unary
    assert(work(enabled, mutable) == 1, 'work estimate cached the node kind or previous arguments')
    mutable.extra = { correlated: true, native_subplan: JedNative::QueryPlans::Node.new(cost: 7) }
    assert(work(enabled, mutable) == 8, 'new correlated subplan metadata was ignored')
    mutable.extra[:native_subplan].cost = 11
    assert(work(enabled, mutable) == 12, 'mutated subplan work stayed stale')
    mutable.extra = nil
    assert(work(enabled, mutable) == 1, 'removed subplan metadata stayed cached')
  end

  def fallback_shapes
    enabled, disabled = engines
    branch = expression(:unary, args: [expression])
    [OtherExpression.new(kind: :binary, type: 'i32', value: '+', args: [branch]),
     expression(:case, args: [[branch, branch], branch]),
     expression(:array, args: [branch]), expression(:call, value: 'abs', args: [branch]),
     expression(:binary, args: false), expression(:binary, args: 'ab'),
     expression(:quantified, args: []), expression(:host_call, value: nil, args: [])].each do |node|
      actual, calls, = original_calls { helper_outcome(enabled, node) }
      expected = helper_outcome(disabled, node)
      assert(actual == expected && calls[JedNative::HostFunctions].positive?,
        'unsupported work node changed or bypassed original overrides')
    end
    [ObservedArray.new([branch, [branch]]), Arguments.new([branch, [branch]])].each do |args|
      node = expression(:binary, args: args)
      before = args.sums.to_i
      actual = helper_outcome(enabled, node)
      assert(args.sums.to_i > before && actual == helper_outcome(disabled, node),
        'non-exact argument array lost its original sum dispatch')
    end
    array = ObservedArray.new([branch, branch])
    assert(work(enabled, array) == 2 && array.sums.positive?, 'Array subclass root lost original sum')
  end

  def metadata_tail
    enabled, disabled = engines
    branch = expression(:unary, args: [expression])
    [nil, {}, { integer_kernel: true }, false, true, [], 'metadata', Object.new].each do |extra|
      node = expression(:binary, args: [branch], extra: extra)
      actual, calls, = original_calls { work(enabled, node) }
      assert(actual == 2 && actual == work(disabled, node) && calls[JedNative::QueryPlans].zero?,
        'ordinary scalar metadata lost its direct work path or changed the estimate')
    end

    # Correlated subplan work runs after all child work. Preserve Hash subclass
    # dispatch, default-proc effects, short-circuit reads and the second lookup
    # of :native_subplan; caching its first result would change this contract.
    %i[missing uncorrelated correlated changing_default replaced_extra failing_lookup failing_cost].each do |scenario|
      observations = [enabled, disabled].map do |engine|
        events = []
        child = expression(:host_call, value: CostProbe.new(events, :child, 3), args: [])
        node = expression(:binary, args: [child])
        post = CostProbe.new(events, :post, 7, failure: scenario == :failing_cost)
        node.extra = case scenario
                     when :missing then ObservedHash.new(events, correlated: true)
                     when :uncorrelated then ObservedHash.new(events, native_subplan: post, correlated: false)
                     when :correlated, :failing_cost
                       ObservedHash.new(events, native_subplan: post, correlated: true)
                     when :changing_default
                       reads = 0
                       Hash.new do |_hash, key|
                         events << key
                         if key == :correlated
                           true
                         else
                           reads += 1
                           reads == 1 ? CostProbe.new(events, :unused_first_cost, 100) : post
                         end
                       end
                     when :replaced_extra
                       Hash.new do |_hash, key|
                         events << key
                         node.extra = ObservedHash.new(events, native_subplan: post, correlated: true)
                         post
                       end
                     when :failing_lookup
                       Hash.new do |_hash, key|
                         events << key
                         raise JedNative::Error.new('22000', 'metadata lookup') if key == :correlated
                         post
                       end
                     end
        [helper_outcome(engine, node), events]
      end
      assert(observations[0] == observations[1], "scalar metadata tail changed #{scenario} lookup, cost or error order")
      answer, events = observations.first
      expected_reads = case scenario
                       when :missing then %i[child native_subplan]
                       when :uncorrelated, :failing_lookup then %i[child native_subplan correlated]
                       else %i[child native_subplan correlated native_subplan post]
                       end
      assert(events == expected_reads, "metadata fixture did not observe its expected #{scenario} sequence")
      if %i[failing_lookup failing_cost].include?(scenario)
        assert(answer[0, 3] == [:error, JedNative::Error, '22000'], 'metadata failure did not remain observable')
      else
        assert(answer == [:value, %i[missing uncorrelated].include?(scenario) ? 4 : 11],
          'metadata fixture work differs from child plus correlated subplan')
      end
    end
  end

  def contextual_overrides
    enabled, disabled = engines
    leaf = expression
    branch = expression(:unary, args: [leaf])
    array = expression(:array, args: [branch, leaf])
    quantified = expression(:quantified, args: [leaf, array])
    assert(work(enabled, quantified) == 5 && work(disabled, quantified) == 5, 'known constructor length changed')
    parameter = expression(:param, value: 0, type: 'i32[]')
    quantified.args[1] = parameter
    [[1], [1, 2, 3], [], nil].each do |binding|
      [enabled, disabled].each { |engine| engine.instance_variable_set(:@params, [binding]) }
      expected = 1 + (binding.is_a?(Array) ? binding.length : 0)
      assert(work(enabled, quantified) == expected && work(disabled, quantified) == expected,
        'quantified work retained a previous array binding')
    end
    quantified.args[1] = expression(:column, value: 0, type: 'i32[]')
    assert(work(enabled, quantified) == work(disabled, quantified) && work(enabled, quantified) == 1,
      'unknown array length gained extra work')

    rows = [[1], [2]]
    subplan = JedNative::QueryPlans::Node.new(cost: 9)
    membership = expression(:query_membership, args: [branch],
      extra: { rows: rows, correlated: false, native_subplan: subplan })
    [nil, false, true, false].each do |context|
      [enabled, disabled].each { |engine| engine.instance_variable_set(:@native_estimate_membership, context) }
      expected = context ? 2 : 2 + rows.length
      assert(work(enabled, membership) == expected && work(disabled, membership) == expected,
        'membership work ignored its current planner context')
    end
    rows << [3]
    assert(work(enabled, membership) == 5, 'membership work retained old materialized row count')
    membership.extra[:correlated] = true
    assert(work(enabled, membership) == 11 && work(disabled, membership) == 11,
      'correlated membership charged materialized rows or omitted its subplan')
    [leaf, branch, expression(:call, value: 'sum', args: [branch])].each do |node|
      node = node.dup
      node.extra = { correlated: true, native_subplan: subplan }
      expected = node.kind == :unary ? 10 : 9
      assert(work(enabled, node) == expected && work(disabled, node) == expected,
        'subplan work applies to every node kind, including leaves and aggregate calls')
    end
  end

  def order_and_errors
    enabled, disabled = engines
    events = []
    first = expression(:host_call, value: CostProbe.new(events, :first, 3), args: [])
    last = expression(:host_call, value: CostProbe.new(events, :last, 5), args: [])
    post = CostProbe.new(events, :post, 7)
    root = expression(:binary, args: [first, expression(:unary, args: [last],
      extra: { correlated: true, native_subplan: post })])
    [enabled, disabled].each do |engine|
      events.clear
      assert(work(engine, root) == 17 && events == %i[first last post], 'special-child work moved or repeated')
    end
    parameter = expression(:param, value: 0, type: 'i32[]')
    middle = expression(:host_call, value: CostProbe.new(events, :middle, 2), args: [])
    quantified = expression(:quantified, args: [middle, parameter])
    ordered = expression(:binary, args: [first, quantified, last])
    [enabled, disabled].each do |engine|
      engine.instance_variable_set(:@params, [[1, 2]])
      engine.define_singleton_method(:plan_constant) do |source|
        events << :bound if source.equal?(parameter)
        super(source)
      end
      events.clear
      assert(work(engine, ordered) == 14 && events == %i[first middle bound bound last],
        'quantified parameter reads moved before child work, after a sibling, or repeated')
    end
    failing = expression(:host_call, value: CostProbe.new(events, :failure, 0, failure: true), args: [])
    aggregate = expression(:call, value: 'SUM', args: [failing])
    [enabled, disabled].each do |engine|
      events.clear
      assert(work(engine, aggregate).zero? && events.empty?, 'aggregate estimator visited its argument subtree')
      failure = helper_outcome(engine, expression(:binary, args: [first, failing, last]))
      assert(failure[0, 3] == [:error, JedNative::Error, '22000'] && events == %i[first failure],
        'scalar work evaluated a later sibling after failure or repeated an earlier sibling')
    end
    first.value.amount = 13
    assert(work(enabled, first) == 13 && work(disabled, first) == 13, 'host declared work stayed stale')
  end

  def deep_guard
    enabled, disabled = engines
    deep = expression
    240.times { deep = expression(:binary, value: 'or', args: [expression, deep]) }
    [enabled, disabled].each do |engine|
      engine.instance_variable_set(:@native_expression_work_depth, 7)
      actual, _, fibers = original_calls { work(engine, deep) }
      assert(actual == 240 && fibers > 1 && engine.instance_variable_get(:@native_expression_work_depth) == 7,
        'deep scalar work lost slicing or failed to restore its caller depth')
      events = []
      failure = expression(:host_call, value: CostProbe.new(events, :deep_failure, 0, failure: true), args: [])
      48.times { failure = expression(:unary, args: [failure]) }
      assert(helper_outcome(engine, failure)[0, 3] == [:error, JedNative::Error, '22000'] &&
        events == [:deep_failure] && engine.instance_variable_get(:@native_expression_work_depth) == 7,
        'deep work error changed order or leaked its slicing depth')
      assert(work(engine, expression(:unary)).eql?(1), 'work estimator did not recover after a deep failure')
    end
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

  def compare(database, sql, params = [], api:, setting: nil, limit: nil)
    enabled, disabled = database.session, database.session
    disabled.engine.instance_variable_set(:@native_planner_work_disabled, true)
    # Keep physical selection visible on every warmed call. This does not
    # disable resolved-expression reuse or any execution compiler.
    [enabled, disabled].each { |session| session.engine.instance_variable_set(:@native_join_physical_disabled, true) }
    statements = [enabled, disabled].map { |session| session.prepare(sql) }
    2.times do
      assert(outcome(enabled, statements[0], sql, params, api) == outcome(disabled, statements[1], sql, params, api),
        "warm work estimator changed #{sql}")
    end
    [enabled, disabled].each do |session|
      session.public_send("#{setting}=", setting == :lifetime_max_cost ? session.lifetime_cost + limit : limit) if setting
    end
    actual = outcome(enabled, statements[0], sql, params, api)
    expected = outcome(disabled, statements[1], sql, params, api)
    assert(actual == expected, "work estimator changed #{api}/#{setting}=#{limit} #{sql}: #{actual.inspect} != #{expected.inspect}")
    actual
  ensure
    enabled&.close
    disabled&.close
  end

  def setup(database)
    database.execute_script(<<~SQL)
      CREATE TABLE pw_l(id i32 PRIMARY KEY,k i32);
      CREATE TABLE pw_r(id i32 PRIMARY KEY,k i32);
      INSERT INTO pw_l VALUES(1,1),(2,1),(3,2),(4,3);
      INSERT INTO pw_r VALUES(10,1),(11,2),(12,2),(13,4);
    SQL
  end

  def public_queries(database)
    sqls = [
      ['SELECT l.id,r.id FROM pw_l l LEFT JOIN pw_r r ON l.k=r.k AND r.id>$1 ORDER BY l.id,r.id', [10]],
      ['SELECT l.id,r.id,c.id FROM pw_l l JOIN pw_r r ON l.k=r.k LEFT JOIN pw_l c ON c.id=l.id WHERE l.id>=$1 ORDER BY l.id,r.id', [1]],
      ['SELECT id FROM pw_l WHERE id=ANY($1) ORDER BY id', [[1, 3, nil]]],
      ['SELECT id FROM pw_l WHERE id IN(SELECT k FROM pw_r) ORDER BY id', []],
      ['SELECT id,(SELECT count(*) FROM pw_r WHERE pw_r.k=pw_l.k) FROM pw_l ORDER BY id', []],
      ['SELECT k,sum(id),count(*) FROM pw_l GROUP BY k HAVING sum(id)>$1 ORDER BY k', [0]],
      ['SELECT id,CASE WHEN k=1 THEN id+1 ELSE id*2 END FROM pw_l ORDER BY id', []],
      ['SELECT id,pw_bump(k) FROM pw_l ORDER BY id', []]
    ]
    APIS.each do |api|
      sqls.each do |sql, params|
        result = compare(database, sql, params, api: api)
        assert(result.first == :rows, 'planner work query fixture failed before exercising its estimate')
        callbacks = @host_calls.length
        plan = compare(database, "EXPLAIN #{sql}", params, api: api)
        assert(plan.first == :rows && @host_calls.length == callbacks, 'planner work EXPLAIN fixture failed or invoked a host body')
      end
      reference = compare(database, 'EXPLAIN SELECT id FROM pw_l WHERE k=1', api: api)
      assert(reference[1] == [[0, 'Filter', 'conjuncts=1', 1, 10],
        [1, 'Scan pw_l', 'Full scan; touched=2', 4, 5]], 'shared explain_estimates reference plan changed')
      sql = 'SELECT l.id,r.id FROM pw_l l JOIN pw_r r ON l.k=r.k WHERE l.id=$1 ORDER BY r.id'
      full = compare(database, sql, [1], api: api)
      assert(full[0, 2] == [:rows, [[1, 10]]], 'work-budget join fixture changed')
      ceilings = api.to_s.end_with?('_prepared') ? (1..full[4] + 1).to_a : [1, full[4], full[4] + 1].uniq
      ceilings.each do |limit|
        %i[max_cost lifetime_max_cost].each { |setting| compare(database, sql, [1], api: api, setting: setting, limit: limit) }
      end
      failing = 'SELECT id,10/(k-$1) FROM pw_l WHERE id<=3 ORDER BY id'
      failure = compare(database, failing, [2], api: api)
      assert(failure[0] == :error && failure[2] == '22012', 'work-error fixture failed to raise')
      [1, 3, failure[-2], failure[-2] + 1].uniq.each do |limit|
        compare(database, failing, [2], api: api, setting: :max_cost, limit: limit)
      end
    end
  end

  def run
    scalar_shapes
    fallback_shapes
    metadata_tail
    contextual_overrides
    order_and_errors
    deep_guard
    @host_calls = []
    environment = JedNative::Environment.new.register_function(name: 'pw_bump', args: ['i32'], returns: 'i32',
      cost: 7, volatility: :immutable) { |value| @host_calls << value; value + 1 }
    JedNative.memory(environment: environment) { |database| setup(database); public_queries(database) }
    Dir.mktmpdir('jed-planner-work') do |directory|
      JedNative.create(File.join(directory, 'fixture.jed'), locking: :exclusive, environment: environment) do |database|
        setup(database)
        public_queries(database)
      end
    end
    puts 'ruby-native planner work: mutable scalars, contextual fallback, dispatch order, depth guards, plans and exact cost prefixes passed'
  end
end

NativePlannerWorkTests.run
