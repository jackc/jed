# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

# P8 defines join ordering and cost. These probes observe the candidate/frontier
# boundary and estimator visitation, which SQL transcripts cannot expose.
module NativeNWayFrontierTests
  APIS = %i[query_prepared execute_prepared query execute].freeze
  MAX = JedNative::QueryPlans::MAX_ESTIMATE
  module_function

  def assert(value, message) = (raise message unless value)

  def image(value)
    case value
    when JedNative::Expression then image(value.to_h)
    when JedNative::CollatedText then [value.to_s, value.collation_label&.to_h]
    when Array then value.map { |child| image(child) }
    when Hash then value.to_h { |key, child| [key, image(child)] }
    else value
    end
  end

  def access_image(access)
    image(access&.slice(:kind, :name, :scan_cost, :scan_rows, :output_rows, :logical_rows,
      :pipeline_cost, :order, :parts, :arms, :terms))
  end

  def state_image(state)
    return unless state
    image(state.slice(:order, :mask, :ordered, :target, :cost, :rows, :logical,
      :output_rows, :join_cost)).merge(
      accesses: state[:accesses].map { |access| access_image(access) },
      steps: state[:steps].map do |step|
        image(step.slice(:algorithm, :kind, :keys, :ons, :gates, :work, :input_rows,
          :rows, :logical, :pairs, :prefix_cost, :cost, :calls)).merge(
          access: access_image(step[:access]), index: access_image(step.dig(:index, :access)))
      end)
  end

  def frontier_image(frontier)
    frontier&.keys&.sort&.map { |key| [key, frontier[key].map { |state| state_image(state) }] }
  end

  def traced
    counts, frontiers, plans = Hash.new(0), [], []
    trace = TracePoint.new(:call, :return) do |event|
      next unless event.defined_class == JedNative::NWayJoins
      if event.event == :call
        counts[event.method_id] += 1
      elsif event.method_id == :nway_strictly_dominated?
        counts[:early_rejections] += 1 if event.return_value
      elsif event.method_id == :nway_island
        frontier = event.binding.local_variable_get(:frontier)
        frontiers << frontier_image(frontier)
      elsif event.method_id == :nway_join_plan
        plans << state_image(event.return_value)
      end
    end
    result = trace.enable { yield }
    [result, counts, frontiers, plans]
  ensure
    trace&.disable
  end

  def access(cost = 3, rows = 2, logical = rows, name = nil, ordered: false)
    { kind: :full, name: name, pipeline_cost: cost, scan_cost: cost,
      scan_rows: rows, output_rows: rows, logical_rows: logical, test_ordered: ordered }
  end

  def state(cost: 10, rows: 2, logical: 2, order: [0, 1], ordered: false,
            selected: access, algorithm: :nested)
    { mask: order.reduce(0) { |mask, ordinal| mask | 1 << ordinal }, order: order,
      ordered: ordered, cost: cost, rows: rows, logical: logical,
      accesses: order.map { selected }, steps: order.drop(1).map { { algorithm: algorithm } } }
  end

  def scalar_dominance
    engine = JedNative::Engine.new
    candidates = [[0, 0, 0], [1, 2, 3], [MAX, MAX, MAX]]
    candidates.each do |triple|
      [-1, 0, 1].repeated_permutation(3) do |delta|
        prior = triple.zip(delta).map { |value, change| (value + change).clamp(0, MAX) }
        states = [state(cost: prior[0], rows: prior[1], logical: prior[2])].freeze
        expected = prior.zip(triple).all? { |a, b| a <= b } && prior.zip(triple).any? { |a, b| a < b }
        actual = engine.send(:nway_strictly_dominated?, states, *triple)
        assert(actual == expected, "strict frontier comparison changed #{prior.inspect}/#{triple.inspect}")
      end
    end
    assert(!engine.send(:nway_strictly_dominated?, [], 0, 0, 0), 'empty frontier dominated a candidate')

    # Equal estimates still need the ordinary canonical sequence/access/algorithm
    # tie breaker, in either insertion order. A strict precheck cannot decide it.
    pairs = [
      [state(order: [0, 1]), state(order: [1, 0])],
      [state(selected: access(3, 2, 2, 'a')), state(selected: access(3, 2, 2, 'z'))],
      [state(algorithm: :nested), state(algorithm: :hash)]
    ]
    pairs.each do |choices|
      expected = choices.min_by { |candidate| engine.send(:nway_state_key, candidate) }
      [choices, choices.reverse].each do |sequence|
        frontier = {}
        sequence.each do |candidate|
          bucket = [candidate[:mask], candidate[:ordered] ? 1 : 0]
          assert(!engine.send(:nway_strictly_dominated?, frontier[bucket] || [],
            candidate[:cost], candidate[:rows], candidate[:logical]), 'exact tie was prematurely rejected')
          engine.send(:nway_retain, frontier, candidate)
        end
        assert(frontier.values.flatten == [expected], 'canonical equal-estimate winner changed')
      end
    end
    tradeoffs = [state(cost: 1, rows: 9, logical: 5), state(cost: 2, rows: 1, logical: 5),
      state(cost: 2, rows: 9, logical: 1)]
    frontier = {}
    tradeoffs.each { |candidate| engine.send(:nway_retain, frontier, candidate) }
    assert(frontier.values.flatten.length == 3, 'frontier lost an incomparable estimate dimension')
  end

  module SyntheticPlanning
    attr_accessor :visits, :fail_work
    def join_driver_ordered?(_statement, _source, access) = access[:test_ordered]
    def nway_extension_inputs(context, state, ordinal, fixed:)
      visits << [:inputs, state[:order].dup, ordinal, fixed]
      [[], [2, 3], [], nil, fixed ? :left : :inner, [], []]
    end
    def estimate_join_rows(rows, expression, _scope, _sources)
      visits << [:estimate, rows, expression]
      (rows + expression - 1) / expression
    end
    def expression_work(expression)
      visits << [:work, expression]
      raise JedNative::Error.new('22012', 'ordered estimator fixture fault') if expression == fail_work
      expression
    end
  end

  def synthetic(disabled)
    JedNative::Engine.new.tap do |engine|
      engine.singleton_class.prepend(SyntheticPlanning)
      engine.visits = []
      engine.instance_variable_set(:@native_nway_frontier_disabled, disabled)
    end
  end

  def context(width)
    { sources: Array.new(width) { |i| { name: "n#{i}" } }, scopes: Array.new(width) { [] },
      scope: [], predicates: [], where: nil, statement: nil,
      available: Array.new(width) { [access(2, 2), access(7, 5), access(4, 2, 2, 'ordered', ordered: true)] } }
  end

  def extensions(engine, context, prefix, frontier = nil, **options)
    engine.visits = []
    result = engine.send(:nway_extensions, context, prefix, 1, frontier: frontier, **options)
    [:rows, result, engine.visits.dup]
  rescue JedNative::Error => error
    [:error, error.sqlstate, error.message, engine.visits.dup]
  end

  def extension_boundary
    enabled, disabled = synthetic(false), synthetic(true)
    context = context(2)
    prefix = { order: [0].freeze, mask: 1, accesses: [access].freeze, steps: [].freeze,
      cost: 10, rows: 3, logical: 3, ordered: false }.freeze
    ordinary = extensions(disabled, context, prefix)
    choices = ordinary[1]
    dimensions = %i[cost rows logical]
    minimum = dimensions.to_h { |key| [key, choices.map { |candidate| candidate[key] }.min] }
    prior = choices.first.merge(minimum).merge(cost: minimum[:cost] - 1)
    bucket = [3, 0]
    frontier = { bucket => [prior].freeze }.freeze
    actual, counts, = traced { extensions(enabled, context, prefix, frontier) }
    expected = extensions(disabled, context, prefix, frontier)
    assert(actual[0] == :rows && actual[1].empty? && expected[1].length == choices.length &&
      counts[:early_rejections] == choices.length && actual[-1] == expected[-1],
      'early rejection skipped estimates/work or materialized a dominated vector')
    assert(frontier[bucket] == [prior] && prefix[:order] == [0] && prefix[:steps].empty?,
      'early dominance mutated a prefix or frontier')
    [{ [7, 0] => [prior] }, { [3, 1] => [prior] }].each do |other_bucket|
      actual = extensions(enabled, context, prefix, other_bucket)
      assert(actual[1].map { |candidate| state_image(candidate) } == choices.map { |candidate| state_image(candidate) },
        'a different subset/order bucket pruned a candidate')
    end
    choices.each do |candidate|
      assert(!candidate[:order].equal?(prefix[:order]) && !candidate[:accesses].equal?(prefix[:accesses]) &&
        !candidate[:steps].equal?(prefix[:steps]), 'surviving candidate aliased its prefix vectors')
    end
    # The first new choice replaces this prior. Later choices that the prior
    # dominated remain dominated transitively; batch creation must not change
    # the eventual frontier when that original witness disappears.
    witness = choices.first.merge(cost: choices.first[:cost] + 1)
    actual_frontier, expected_frontier = [{ bucket => [witness] }, { bucket => [witness] }]
    kept, counts, = traced { extensions(enabled, context, prefix, actual_frontier) }
    unfiltered = extensions(disabled, context, prefix, expected_frontier)
    kept[1].each { |candidate| enabled.send(:nway_retain, actual_frontier, candidate) }
    unfiltered[1].each { |candidate| disabled.send(:nway_retain, expected_frontier, candidate) }
    assert(counts[:early_rejections].positive? && !actual_frontier[bucket].include?(witness) &&
      frontier_image(actual_frontier) == frontier_image(expected_frontier),
      'replacing a batch dominator changed the retained frontier')
    tie = choices.first.merge(order: [1, 0])
    tied = extensions(enabled, context, prefix, { bucket => [tie] })
    assert(tied[1].any? { |candidate| state_image(candidate) == state_image(choices.first) },
      'equal-estimate candidate never reached its ordinary structural tie break')
    [2, 3].each do |failure|
      enabled.fail_work = disabled.fail_work = failure
      actual = extensions(enabled, context, prefix, frontier)
      expected = extensions(disabled, context, prefix, frontier)
      assert(actual == expected && actual[0] == :error, 'dominated choice suppressed or reordered an estimator fault')
    end
  end

  def synthetic_islands
    [3, 5, 8, 9].each do |width|
      context = context(width)
      enabled, disabled = synthetic(false), synthetic(true)
      actual, calls, frontiers, = traced { enabled.send(:nway_island, context, (0...width).to_a, nil, final: false) }
      expected, old_calls, old_frontiers, = traced { disabled.send(:nway_island, context, (0...width).to_a, nil, final: false) }
      assert(state_image(actual) == state_image(expected) && frontiers == old_frontiers &&
        enabled.visits == disabled.visits, "frontier/estimate visitation changed at #{width} inputs")
      if width <= 8
        assert(calls[:early_rejections].positive? && calls[:nway_retain] < old_calls[:nway_retain],
          "#{width}-input synthetic DP did not avoid candidate retention calls")
      else
        assert(calls[:nway_strictly_dominated?].zero? && calls[:early_rejections].zero? && frontiers == [nil],
          'greedy larger-island fallback acquired a DP frontier')
      end
    end
  end

  def setup(database)
    database.execute_script(<<~SQL)
      CREATE TABLE nf_a(id i32 PRIMARY KEY,k i32,v i32);
      CREATE TABLE nf_b(id i32 PRIMARY KEY,k i32,v i32);
      CREATE TABLE nf_c(id i32 PRIMARY KEY,k i32,v i32);
      CREATE TABLE nf_d(id i32 PRIMARY KEY,k i32,padding text);
      INSERT INTO nf_a VALUES(1,1,10),(2,2,20),(3,9,30);
      INSERT INTO nf_b VALUES(1,1,100),(2,2,200),(4,4,400);
      INSERT INTO nf_c VALUES(1,1,7),(2,2,8),(3,1,9);
      INSERT INTO nf_d VALUES(1,1,'first'),(2,2,'second');
      CREATE INDEX nf_b_k ON nf_b(k);
      CREATE INDEX nf_c_k ON nf_c(k);
    SQL
  end

  def sessions(database, fresh: true)
    [false, true].map do |disabled|
      database.session.tap do |current|
        current.engine.instance_variable_set(:@native_nway_frontier_disabled, disabled)
        current.engine.instance_variable_set(:@native_join_physical_disabled, fresh)
      end
    end
  end

  def outcome(current, statement, sql, params, api)
    prior, rows, checkpoints, phase = current.lifetime_cost, [], [], :admission
    result = api.to_s.end_with?('_prepared') ? current.public_send(api, statement, *params) : current.public_send(api, sql, *params)
    names, types, phase = result.columns, result.column_types, :pull
    result.each do |row|
      rows << image(row.to_a)
      checkpoints << [result.cost, current.lifetime_cost - prior]
    end
    [:rows, rows, names, types, checkpoints, result.cost, current.lifetime_cost - prior]
  rescue JedNative::Error => error
    [:error, phase, error.sqlstate, error.message, rows, checkpoints, result&.cost || current.engine.cost, current.lifetime_cost - prior]
  ensure
    result&.close
  end

  def compare(current, statements, sql, params, api, trace: false)
    if trace
      actual, calls, frontiers, plans = traced { outcome(current[0], statements[0], sql, params, api) }
      expected, old_calls, old_frontiers, old_plans = traced { outcome(current[1], statements[1], sql, params, api) }
      assert(frontiers == old_frontiers && plans == old_plans, 'early dominance changed a complete frontier or physical winner')
      @last_calls = [calls, old_calls]
    else
      actual = outcome(current[0], statements[0], sql, params, api)
      expected = outcome(current[1], statements[1], sql, params, api)
    end
    assert(actual == expected, "frontier preparation changed #{api} #{params.inspect}: #{actual.inspect} != #{expected.inspect}")
    actual
  end

  def public_queries(database)
    current = nil
    queries = [
      'SELECT a.id,b.id,c.id FROM nf_a a JOIN nf_b b ON a.k=b.k JOIN nf_c c ON b.k=c.k WHERE a.id>=$1 ORDER BY a.id,b.id,c.id',
      'SELECT a.id,b.v,c.v,d.padding FROM nf_a a JOIN nf_b b ON b.id=a.k JOIN nf_c c ON c.id=a.id JOIN nf_d d ON d.id=c.id WHERE a.id>=$1 ORDER BY a.id',
      'SELECT a.id,b.v,c.v,d.padding,e.id FROM nf_a a JOIN nf_b b ON b.id=a.k JOIN nf_c c ON c.id=a.id JOIN nf_d d ON d.id=c.id JOIN nf_b e ON e.id=d.id WHERE a.id>=$1 ORDER BY a.id',
      'SELECT a.id,b.id,c.id FROM nf_a a,nf_b b,nf_c c WHERE a.k=b.k AND c.id=a.id AND a.id>=$1 ORDER BY a.id,b.id,c.id',
      'SELECT a.id,b.id,c.id,d.id FROM nf_a a JOIN nf_b b ON b.id=a.k LEFT JOIN nf_c c ON c.id=a.id AND c.k<>2 JOIN nf_d d ON d.id=b.id WHERE a.id>=$1 ORDER BY a.id',
      'SELECT a.id,b.id,c.id,d.id FROM nf_a a LEFT JOIN nf_b b ON b.id=a.k JOIN nf_c c ON c.id=a.id JOIN nf_d d ON d.id=c.id WHERE a.id>=$1 ORDER BY a.id',
      'SELECT a.id,b.id,c.id FROM nf_a a LEFT JOIN nf_b b ON b.id=a.k LEFT JOIN nf_c c ON c.id=b.id WHERE a.id>=$1 ORDER BY a.id',
      'SELECT a.id,b.id,c.id FROM nf_a a JOIN nf_b b ON b.id=a.k JOIN nf_c c ON c.id=a.id WHERE a.id>=$1 ORDER BY a.id LIMIT 1 OFFSET 1',
      'SELECT a.id,b.id,c.id FROM nf_a a JOIN nf_b b ON b.id=a.k JOIN nf_c c ON c.id=a.id WHERE a.id>=$1 ORDER BY a.id LIMIT 0'
    ]
    positive = 0
    APIS.each do |api|
      queries.each do |sql|
        current = sessions(database)
        statements = current.map { |session| session.prepare(sql) }
        [1, 2, nil, 99, 1].each do |value|
          result = compare(current, statements, sql, [value], api, trace: true)
          assert(result.first == :rows, 'frontier public fixture failed')
          positive += @last_calls.first[:early_rejections]
        end
        explain = current.map { |session| outcome(session, nil, "EXPLAIN #{sql}", [1], :execute) }
        assert(explain.first == explain.last, 'frontier optimization changed complete EXPLAIN metadata/costs')
        current.each(&:close)
      end
    end
    assert(positive.positive?, 'public queries never exercised early dominance')
    sql = 'SELECT a.id,b.v,c.v FROM nf_a a JOIN nf_b b ON b.id=a.k JOIN nf_c c ON c.id=a.id WHERE a.id=$1 AND b.id=$1 AND c.id=$1 ORDER BY a.id'
    current = sessions(database, fresh: false)
    statements = current.map { |session| session.prepare(sql) }
    normal_calls = []
    [1, 1, 1, 2, nil, 1].each do |value|
      compare(current, statements, sql, [value], :query_prepared, trace: true)
      normal_calls << [value, @last_calls.map { |calls| calls[:nway_join_plan] }]
    end
    assert(normal_calls.first.last.all?(&:positive?) &&
      normal_calls.any? { |value, calls| value && calls.all?(&:zero?) },
      'ordinary cold/warm controls did not exercise fresh and retained physical plans')
  ensure
    current&.each(&:close)
  end

  def error_prefixes(database)
    current = nil
    queries = [
      'SELECT a.id,10/($1::i32),c.id FROM nf_a a JOIN nf_b b ON b.id=a.k JOIN nf_c c ON c.id=a.id WHERE a.id>=$2 ORDER BY a.id LIMIT 1',
      'SELECT a.id,b.v,c.id FROM nf_a a JOIN nf_b b ON b.id=a.k AND b.v/($1::i32)>0 JOIN nf_c c ON c.id=a.id WHERE a.id>=$2 ORDER BY a.id LIMIT 1'
    ]
    APIS.product(queries).each do |api, sql|
      [[2, 1], [0, 1], [0, 99]].each do |params|
        current = sessions(database)
        statements = current.map { |session| session.prepare(sql) }
        complete = compare(current, statements, sql, params, api)
        total = complete[-2]
        %i[max_cost lifetime_max_cost].each do |setting|
          (1..total + 1).each do |limit|
            current.each do |session|
              session.max_cost = session.lifetime_max_cost = 0
              session.public_send("#{setting}=", setting == :lifetime_max_cost ? session.lifetime_cost + limit : limit)
            end
            compare(current, statements, sql, params, api)
          end
        end
        current.each(&:close)
      end
    end
  ensure
    current&.each(&:close)
  end

  def run
    scalar_dominance
    extension_boundary
    synthetic_islands
    [false, true].each do |disk|
      Dir.mktmpdir('jed-nway-frontier-') do |directory|
        database = disk ? JedNative.create(File.join(directory, 'fixture.jed'), locking: :exclusive, work_mem: 1024) : JedNative.memory
        setup(database)
        public_queries(database)
        error_prefixes(database)
      ensure
        database&.close
      end
    end
    puts 'ruby-native nway frontier: dominance, ties, plans and costs passed'
  end
end

NativeNWayFrontierTests.run
