# frozen_string_literal: true

require 'tmpdir'
require 'weakref'
require_relative '../lib/jed_native'

# Shared transcripts define indexed JOIN semantics. These host probes observe
# execution-local bound structure while row values, cursors and catalogs change.
module NativePreparedJoinBoundTests
  APIS = %i[query_prepared execute_prepared query execute].freeze
  module_function

  def assert(value, message) = (raise message unless value)

  def setup(database)
    database.execute_script(<<~SQL)
      CREATE TABLE jb_o(id i32 PRIMARY KEY,k i32,g i32,wide i64,d decimal);
      CREATE TABLE jb_r(id i16 PRIMARY KEY,k i32,g i32,d decimal,flag boolean,label text);
      CREATE TABLE jb_p(id i32 PRIMARY KEY,k i32,flag boolean);
      CREATE TABLE jb_e(id i32 PRIMARY KEY,k i32);
      CREATE TABLE jb_c(x i16,y i32,label text,PRIMARY KEY(x,y));
      INSERT INTO jb_o VALUES(1,2,1,2,2.0),(2,5,2,40000,2.5),(3,99,0,-32769,99),(4,NULL,1,NULL,NULL),(5,2,0,5,5.0);
      INSERT INTO jb_r VALUES #{32.times.map { |i| "(#{i},#{i % 8},#{i % 3},#{i}.0,#{i.odd? ? 'TRUE' : 'FALSE'},'v#{i}')" }.join(',')};
      INSERT INTO jb_p VALUES #{32.times.map { |i| "(#{i},#{i % 8},#{i.odd? ? 'TRUE' : 'FALSE'})" }.join(',')};
      INSERT INTO jb_e VALUES #{32.times.map { |i| "(#{i},#{i})" }.join(',')};
      INSERT INTO jb_c VALUES #{32.times.map { |i| "(#{i / 4},#{i % 4},'c#{i}')" }.join(',')};
      CREATE INDEX jb_r_k ON jb_r(k);
      CREATE INDEX jb_r_kg ON jb_r(k,g);
      CREATE INDEX jb_r_d ON jb_r(d);
      CREATE INDEX jb_p_k ON jb_p(k) WHERE flag=TRUE;
      CREATE INDEX jb_e_expr ON jb_e((k+1));
      CREATE TABLE jb_fixed_o(id i32 PRIMARY KEY,k i32);
      CREATE TABLE jb_fixed_r(id i32 PRIMARY KEY,v i32);
      INSERT INTO jb_fixed_o VALUES(1,2),(2,5),(3,9),(4,NULL);
      INSERT INTO jb_fixed_r VALUES(2,20),(4,40),(5,50);
    SQL
  end

  def session(database, disabled: false)
    database.session.tap { |current| current.engine.instance_variable_set(:@native_join_bounds_disabled, disabled) }
  end

  def image(value)
    case value
    when Array then value.map { |child| image(child) }
    when JedNative::CollatedText then [value.class.name, value.to_s, value.collation_label&.to_h]
    else value
    end
  end

  def outcome(current, statement, sql, params, api, collect: false)
    prior, phase, rows, checkpoints = current.lifetime_cost, :admission, [], []
    result = api.to_s.end_with?('_prepared') ? current.public_send(api, statement, *params) : current.public_send(api, sql, *params)
    names, types, phase = result.columns, result.column_types, :pull
    if result.is_a?(JedNative::Rows) && !collect
      result.each do |row|
        rows << image(row.to_a)
        checkpoints << [result.cost, current.lifetime_cost - prior]
      end
    else
      rows = image(result.rows)
    end
    [:rows, rows, names, types, checkpoints, result.cost, current.lifetime_cost - prior]
  rescue JedNative::Error => error
    [:error, phase, error.sqlstate, error.message, rows, checkpoints,
     result&.cost || current.engine.cost, current.lifetime_cost - prior]
  ensure
    result&.close
  end

  def traced
    counts = Hash.new(0)
    tracker = TracePoint.new(:call) do |event|
      counts[:probes] += 1 if event.defined_class == JedNative::IndexNestedLoops && event.method_id == :join_index_relation
      counts[:comparisons] += 1 if event.defined_class == JedNative::IndexScans && event.method_id == :key_comparisons
      counts[:spool] += 1 if event.method_id == :spill_index_join && event.defined_class == JedNative::SpillSelect
      counts[:prepare] += 1 if event.method_id == :prepare_join_index_execution
    end
    value = tracker.enable { yield }
    [value, counts]
  ensure
    tracker&.disable
  end

  def compare_current(sessions, statements, sql, params, api, trace: false, **options)
    if trace
      actual, enabled = traced { outcome(sessions[0], statements[0], sql, params, api, **options) }
      expected, disabled = traced { outcome(sessions[1], statements[1], sql, params, api, **options) }
      @last_counts = [enabled, disabled]
    else
      actual = outcome(sessions[0], statements[0], sql, params, api, **options)
      expected = outcome(sessions[1], statements[1], sql, params, api, **options)
    end
    assert(actual == expected, "bound preparation changed #{api} #{sql} #{params.inspect}: #{actual.inspect} != #{expected.inspect}")
    actual
  end

  def pair(database, sql)
    sessions = [session(database), session(database, disabled: true)]
    [sessions, sessions.map { |current| current.prepare(sql) }]
  end

  def compare(database, sql, params, api:, warm: params, setting: nil, limit: 0, **options)
    sessions, statements = pair(database, sql)
    2.times { compare_current(sessions, statements, sql, warm, api) }
    sessions.each do |current|
      current.public_send("#{setting}=", setting == :lifetime_max_cost ? current.lifetime_cost + limit : limit) if setting
    end
    compare_current(sessions, statements, sql, params, api, **options)
  ensure
    sessions&.each(&:close)
  end

  def query_equivalence(database)
    sessions = nil
    cases = [
      'SELECT o.id,r.id FROM jb_o o JOIN jb_r r ON r.id=o.k WHERE o.id>=$1 ORDER BY o.id,r.id',
      'SELECT o.id,r.id FROM jb_o o LEFT JOIN jb_r r ON r.id=o.k WHERE o.id>=$1 ORDER BY o.id,r.id',
      'SELECT o.id,r.id FROM jb_o o LEFT JOIN jb_r r ON r.k=o.k WHERE o.id>=$1 ORDER BY o.id,r.id',
      'SELECT o.id,r.id FROM jb_o o LEFT JOIN jb_r r ON r.k=o.k AND r.g=o.g WHERE o.id>=$1 ORDER BY o.id,r.id',
      'SELECT o.id,c.x,c.y FROM jb_o o LEFT JOIN jb_c c ON c.x=o.k AND c.y=o.g WHERE o.id>=$1 ORDER BY o.id,c.x,c.y',
      'SELECT o.id,p.id FROM jb_o o LEFT JOIN jb_p p ON p.k=o.k AND p.flag=TRUE WHERE o.id>=$1 ORDER BY o.id,p.id',
      'SELECT o.id,e.id FROM jb_o o LEFT JOIN jb_e e ON e.k+1=o.k WHERE o.id>=$1 ORDER BY o.id,e.id',
      'SELECT o.id,r.id FROM jb_o o LEFT JOIN jb_r r ON r.id>=o.k AND r.id<o.k+3 WHERE o.id>=$1 ORDER BY o.id,r.id',
      'SELECT o.id,r.id FROM jb_o o LEFT JOIN jb_r r ON r.id=o.k AND r.id>3 WHERE o.id>=$1 ORDER BY o.id,r.id',
      'SELECT o.id,r.id FROM jb_o o LEFT JOIN jb_r r ON r.id=o.k AND r.id=o.g WHERE o.id>=$1 ORDER BY o.id,r.id',
      'SELECT o.id,r.id FROM jb_o o LEFT JOIN jb_r r ON r.id=o.k OR r.id=o.g WHERE o.id>=$1 ORDER BY o.id,r.id',
      'SELECT o.id,r.id FROM jb_o o LEFT JOIN jb_r r ON r.id=o.wide WHERE o.id>=$1 ORDER BY o.id,r.id',
      'SELECT o.id,r.id FROM jb_o o LEFT JOIN jb_r r ON r.id=o.d WHERE o.id>=$1 ORDER BY o.id,r.id',
      'SELECT o.id,r.id FROM jb_o o LEFT JOIN jb_r r ON r.d=o.k WHERE o.id>=$1 ORDER BY o.id,r.id',
      'SELECT o.id,r.id FROM jb_o o LEFT JOIN jb_r r ON r.d<o.k WHERE o.id>=$1 ORDER BY o.id,r.id'
    ]
    APIS.each do |api|
      cases.each do |sql|
        sessions, statements = pair(database, sql)
        [1, 3, 99, nil, 1].each do |value|
          answer = compare_current(sessions, statements, sql, [value], api)
          assert(answer.first == :rows, "bounded JOIN fixture failed: #{answer.inspect}")
        end
        sessions.each(&:close)
      end
    end
  ensure
    sessions&.each(&:close)
  end

  class ProbeRow
    def initialize(values, events, failure = nil)
      @values, @events, @failure = values, events, failure
    end
    def [](index)
      @events << [:read, index]
      raise JedNative::Error.new('XX001', 'bound source read failed') if index == @failure
      @values[index]
    end
  end

  module ObserveProbes
    attr_accessor :bound_events, :nested_bound_probe
    def charge(amount = 1)
      bound_events << [:charge, amount, cost] if bound_events
      if (nested = nested_bound_probe)
        self.nested_bound_probe = nil
        nested.call
      end
      super
    end
    def normalize_scan_bound(type, value)
      bound_events << [:normalize, type, value] if bound_events
      super
    end
  end

  def chosen_plan(engine, on, target: 'jb_r', name: nil, kind: nil)
    statement = JedNative::Statement.new("SELECT 1 FROM jb_o o LEFT JOIN #{target} r ON #{on}").ast
    left = JedNative::Relation.new(columns: engine.table_columns(engine.tables.fetch('jb_o'), 'o'), rows: [])
    edge = statement[:joins].first
    plans = engine.join_index_plan(left, edge[:relation], :left, edge[:on], nil, choose: false, all: true)
    plan = plans&.find { |item| (!name || item[:access][:name] == name) && (!kind || item[:access][:kind] == kind) }
    assert(plan, "internal index fixture has no requested access #{target}/#{name}/#{kind}: #{plans&.map { |item| item[:access].slice(:kind, :name) }.inspect}")
    [left, plan]
  end

  def probe_outcome(engine, plan, left, values, failure: nil, limit: 0, lifetime: 0)
    events = engine.bound_events = []
    engine.instance_variable_set(:@cost, 0)
    engine.instance_variable_set(:@lifetime_cost, 7)
    engine.max_cost, engine.lifetime_max_cost = limit, lifetime.positive? ? 7 + lifetime : 0
    before_outer = engine.instance_variable_get(:@outer).dup
    row = ProbeRow.new(values, events, failure)
    begin
      result = engine.join_index_relation(plan, left, row)
      answer = [:rows, image(result.rows.map(&:to_a))]
    rescue JedNative::Error => error
      answer = [:error, error.sqlstate, error.message]
    ensure
      assert(engine.instance_variable_get(:@outer) == before_outer, 'failed bound probe leaked correlation scope')
    end
    [answer, engine.cost, engine.lifetime_cost, events]
  end

  def internal_probes(database)
    sessions = [session(database), session(database, disabled: true)]
    engines = sessions.map(&:engine)
    engines.each { |engine| engine.singleton_class.prepend(ObserveProbes) }
    cases = [
      ['r.id=o.k', 'jb_r', nil, :primary],
      ['r.id=o.k AND r.id=o.g', 'jb_r', nil, :primary],
      ['r.id=o.k AND r.id>=o.g', 'jb_r', nil, :primary],
      ['r.id<o.k AND r.id>o.g', 'jb_r', nil, :primary],
      ['r.k=o.k AND r.g=o.g', 'jb_r', 'jb_r_kg', :secondary],
      ['r.k=o.k AND r.flag=TRUE', 'jb_p', 'jb_p_k', :secondary],
      ['r.k+1=o.k', 'jb_e', 'jb_e_expr', :secondary],
      ['r.x=o.k AND r.y=o.g', 'jb_c', nil, :primary],
      ['r.id=o.wide', 'jb_r', nil, :primary],
      ['r.id=o.d', 'jb_r', nil, :primary],
      ['r.d=o.k', 'jb_r', 'jb_r_d', :secondary]
    ]
    cases.each do |on, target, name, kind|
      pairs = engines.map { |engine| chosen_plan(engine, on, target: target, name: name, kind: kind) }
      originals = pairs.map(&:last)
      shapes = originals.map { |plan| [plan.keys.dup, plan[:access].keys.dup, plan[:access][:parts]&.map { |part| part.keys.dup }] }
      plans = engines.zip(originals).map { |engine, plan| engine.send(:prepare_join_index_execution, plan) }
      assert(!plans[0].equal?(originals[0]) && !plans[0][:access].equal?(originals[0][:access]) &&
        plans[0][:stored].equal?(originals[0][:stored]), 'bound preparation copied storage or mutated a shared plan')
      state = plans[0].fetch(:native_join_bounds)
      assert(state.equal?(plans[0][:access][:native_join_bounds]) && state.probes.zero? &&
        !state.prepared && state.terms.nil?, 'private access and plan did not share fresh execution state')
      [2, 5, nil, 99, 2].each_with_index do |value, index|
        values = [1, value, index.even? ? 2 : 3, index == 1 ? 40_000 : value, JedNative::Decimal.parse(index == 1 ? '2.5' : '2.0')]
        actual, expected = engines.each_with_index.map do |engine, side|
          probe_outcome(engine, plans[side], pairs[side][0], values)
        end
        assert(actual == expected, "prepared bound altered ordered reads/charges #{on}: #{actual.inspect} != #{expected.inspect}")
        assert(originals.map { |plan| [plan.keys, plan[:access].keys, plan[:access][:parts]&.map(&:keys)] } == shapes,
          'execution-local structure was attached to a shared planner object')
        assert(state.probes == index + 1, 'an indexed probe was skipped or counted twice')
        if index.zero?
          assert(!state.prepared && state.keys.nil? && state.comparisons.nil? && state.units.zero?,
            'first probe eagerly prepared bound structure')
        else
          assert(state.prepared && state.units <= JedNative::IndexNestedLoops::MAX_JOIN_BOUND_UNITS,
            "ordinary bound structure was not admitted within its cap: #{on}")
          lists = state.comparisons.compact + state.empty_comparisons.values
          assert(lists.all? { |list| list.frozen? && list.all?(&:frozen?) } &&
            state.keys.compact.all?(&:frozen?) && (!state.terms || state.terms.frozen?),
            'owned bound arrays/primary keys were not frozen')
        end
      end
      other = engines[0].send(:prepare_join_index_execution, originals[0]).fetch(:native_join_bounds)
      assert(!other.equal?(state) && other.probes.zero? && !other.prepared,
        'a second execution reused the first execution bound state')
      # Preparation holds only structure: disabling it after warming remains
      # authoritative, and a subsequent source fault occurs at the same read.
      engines[0].instance_variable_set(:@native_join_bounds_disabled, true)
      actual, expected = engines.each_with_index.map do |engine, side|
        probe_outcome(engine, plans[side], pairs[side][0], [1, 2, 3, 2, JedNative::Decimal.new(2)], failure: 1)
      end
      assert(actual == expected, 'disable after preparation changed a deferred bound fault')
      engines[0].instance_variable_set(:@native_join_bounds_disabled, false)
    end

    # A tiny equality+range predicate makes every error prefix inexpensive.
    pairs = engines.map { |engine| chosen_plan(engine, 'r.id=o.k AND r.id>=o.g', kind: :primary) }
    plans = engines.zip(pairs).map { |engine, (_, plan)| engine.send(:prepare_join_index_execution, plan) }
    2.times do
      engines.each_with_index { |engine, side| probe_outcome(engine, plans[side], pairs[side][0], [1, 2, 1, 2, nil]) }
    end
    full = probe_outcome(engines[1], plans[1], pairs[1][0], [1, 2, 1, 2, nil])
    (1..full[1] + 1).each do |limit|
      [nil, 1, 2].each do |failure|
        [{ limit: limit }, { lifetime: limit }].each do |budget|
          actual, expected = engines.each_with_index.map do |engine, side|
            probe_outcome(engine, plans[side], pairs[side][0], [1, 2, 1, 2, nil], failure: failure, **budget)
          end
          assert(actual == expected, 'bound preparation changed a cost/source-fault prefix')
        end
      end
    end
    # A nested indexed scan must restore the current outer row. Each scan owns
    # a different structural descriptor even while both execute on one engine.
    inner = engines.map { |engine| chosen_plan(engine, 'r.k=o.k', name: 'jb_r_k', kind: :secondary) }
    inner_plans = engines.zip(inner).map { |engine, (_, plan)| engine.send(:prepare_join_index_execution, plan) }
    2.times do
      engines.each_with_index { |engine, side| probe_outcome(engine, inner_plans[side], inner[side][0], [1, 5, 1, 5, nil]) }
    end
    nested_answers = []
    engines.each_with_index do |engine, side|
      engine.nested_bound_probe = lambda do
        result = engine.join_index_relation(inner_plans[side], inner[side][0], [1, 5, 1, 5, nil])
        nested_answers[side] = image(result.rows.map(&:to_a))
      end
    end
    actual, expected = engines.each_with_index.map do |engine, side|
      probe_outcome(engine, plans[side], pairs[side][0], [1, 2, 1, 2, nil])
    end
    assert(actual == expected && nested_answers[0] == nested_answers[1] && !nested_answers[0].empty?,
      'nested indexed probes mixed bound state, correlation rows or cost order')
    assert(!plans[0][:native_join_bounds].equal?(inner_plans[0][:native_join_bounds]) &&
      inner_plans[0][:native_join_bounds].probes == 3, 'nested probe reused another execution descriptor')
  ensure
    sessions&.each(&:close)
  end

  class UnsizedRows
    include Enumerable
    def initialize(rows) = (@rows = rows)
    def each(&block) = @rows.each(&block)
    def length = raise('bound preparation inspected a lazy source length')
  end

  def source_boundaries(database)
    current = session(database)
    engine = current.engine
    parsed = JedNative::Statement.new('SELECT 1 FROM jb_o o LEFT JOIN jb_r r ON r.id=o.k').ast
    edge = parsed[:joins].first
    [[], [[1, 2, 1, 2, nil]], [[1, 2, 1, 2, nil], [2, 5, 2, 5, nil]]].each do |rows|
      [false, true].each do |unsized|
        left, original = chosen_plan(engine, 'r.id=o.k', kind: :primary)
        left.rows = unsized ? UnsizedRows.new(rows) : rows
        source = edge[:relation].merge(native_index_plan: original)
        states = []
        trace = TracePoint.new(:return) do |event|
          next unless event.method_id == :prepare_join_index_execution
          state = event.return_value[:native_join_bounds]
          states << state if state
        end
        result = trace.enable { engine.join(left, source, :left, edge[:on], nil) }
        assert(result.rows.length == rows.length, 'indexed source boundary changed outer row count')
        if !unsized && rows.length <= 1
          assert(states.empty?, 'zero/singleton exact Array acquired bound preparation')
        else
          assert(states.length == 1 && states.first.probes == rows.length &&
            states.first.prepared == (rows.length >= 2), 'lazy/repeated source prepared at the wrong probe')
        end
        assert(!original.key?(:native_join_bounds) && !original[:access].key?(:native_join_bounds),
          'source execution altered its original access plan')
      ensure
        trace&.disable
      end
    end
  ensure
    current&.close
  end

  def structural_bounds(database)
    current = session(database)
    engine = current.engine
    _, original = chosen_plan(engine, 'r.id=o.k', kind: :primary)
    %i[full primary_interval secondary_interval gin gist].each do |kind|
      plan = original.merge(access: original[:access].merge(kind: kind))
      assert(engine.send(:prepare_join_index_execution, plan).equal?(plan), "unsupported #{kind} acquired state")
    end
    [Hash.new(1).merge(original), original.dup.compare_by_identity,
     Class.new(Hash).new.merge(original)].each do |plan|
      assert(engine.send(:prepare_join_index_execution, plan).equal?(plan), 'unusual plan Hash acquired state')
    end
    leaf = JedNative::Expression.new(kind: :literal, type: 'i32', value: 1)
    max = JedNative::IndexNestedLoops::MAX_JOIN_BOUND_NODES
    assert(engine.send(:join_bound_expressions?, Array.new(max, leaf)) &&
      !engine.send(:join_bound_expressions?, Array.new(max + 1, leaf)),
      'bound expression occurrence cap lost duplicates or an exact boundary')
    deep = leaf
    32.times { deep = JedNative::Expression.new(kind: :unary, type: 'i32', value: '+', args: [deep]) }
    assert(engine.send(:join_bound_expressions?, [deep]), 'exact depth boundary fell back')
    deep = JedNative::Expression.new(kind: :unary, type: 'i32', value: '+', args: [deep])
    assert(!engine.send(:join_bound_expressions?, [deep]), 'excessive bound depth was admitted')
    [leaf.dup.tap { |node| node.type = 'jsonb' },
     leaf.dup.tap { |node| node.extra = Hash.new(1) },
     leaf.dup.tap { |node| node.extra = { type_mod: [1, 2, 3] } },
     Class.new(JedNative::Expression).new(kind: :literal, type: 'i32', value: 1)].each do |node|
      assert(!engine.send(:join_bound_expressions?, [node]), 'unusual bound expression was retained')
    end
    too_many = original.merge(access: original[:access].merge(parts:
      Array.new(JedNative::IndexNestedLoops::MAX_JOIN_BOUND_PARTS + 1, original[:access][:parts].first)))
    local = engine.send(:prepare_join_index_execution, too_many)
    state = local.fetch(:native_join_bounds)
    assert(!engine.send(:prepare_join_bound_structure, local, state) && !state.prepared && state.units.zero?,
      'excessive index parts left partial retained state')
    local = engine.send(:prepare_join_index_execution, original)
    state = local.fetch(:native_join_bounds)
    assert(engine.send(:prepare_join_bound_structure, local, state), 'valid internal bound fixture was rejected')
    part = original[:access][:parts].first
    cap = JedNative::IndexNestedLoops::MAX_JOIN_BOUND_UNITS
    state.units = cap
    expected = engine.send(:key_comparisons, part[:key], part[:terms])
    actual = engine.send(:join_bound_comparisons, state, state.empty_comparisons, part, part[:key], part[:terms])
    assert(actual == expected && state.units == cap && state.empty_comparisons.empty?,
      'full retained-unit allowance changed results or grew the comparison cache')
    engine.send(:join_bound_terms, state, original[:predicate])
    assert(state.units == cap && state.terms.nil?, 'full retained-unit allowance grew the terms cache')
  ensure
    current&.close
  end

  def repeated_and_singleton(database, disk:)
    repeated = 'SELECT o.id,r.v FROM jb_fixed_o o LEFT JOIN jb_fixed_r r ON r.id=o.k ORDER BY o.id'
    singleton = 'SELECT o.id,r.v FROM jb_fixed_o o LEFT JOIN jb_fixed_r r ON r.id=o.k WHERE o.id=$1 ORDER BY o.id'
    APIS.each do |api|
      actual = compare(database, repeated, [], api: api, trace: true)
      assert(actual[0] == :rows && actual[1] == [[1, 20], [2, 50], [3, nil], [4, nil]] && actual[-2] == 16,
        'shared indexed LEFT fixture changed its known result/cost')
      enabled, disabled = @last_counts
      assert(enabled[:probes] > 1 && enabled[:probes] == disabled[:probes] && enabled[:comparisons] < disabled[:comparisons],
        "repeated probes did not reuse bound structure: #{@last_counts.inspect}")
      [1, 3, 4, 99].each do |value|
        compare(database, singleton, [value], api: api, trace: true)
        enabled, disabled = @last_counts
        assert(enabled[:probes] == disabled[:probes], 'singleton/miss preparation changed probe count')
      end
      if disk
        blocked = repeated.sub('SELECT ', 'SELECT DISTINCT ')
        answer = compare(database, blocked, [], api: api, trace: true)
        enabled, disabled = @last_counts
        assert(answer.first == :rows && enabled[:spool].positive? && enabled[:probes] > 1 &&
          enabled[:comparisons] < disabled[:comparisons], 'blocking file fixture did not reuse spill indexed bounds')
      end
    end
  end

  def detached_structure
    JedNative.memory do |database|
      setup(database)
      current = session(database)
      engine = current.engine
      left, original = chosen_plan(engine, 'r.id=o.k', kind: :primary)
      plan = engine.send(:prepare_join_index_execution, original)
      row = [1, 2, 1, 2, nil]
      3.times { engine.join_index_relation(plan, left, row) }
      state = plan.fetch(:native_join_bounds)
      assert(state.prepared && state.comparisons.any?, 'retention fixture never used bound structure')
      references = [database, current, engine, left, row, original, plan].map { |object| WeakRef.new(object) }
      current.close
      [state, references]
    end
  end

  def retention
    state, references = Thread.new { detached_structure }.value
    3.times { GC.start }
    assert(state.prepared && references.none?(&:weakref_alive?),
      'retained bound structure captured a session, database, row or execution plan')
  end

  def budget_prefixes(database)
    sql = 'SELECT o.id,r.v FROM jb_fixed_o o LEFT JOIN jb_fixed_r r ON r.id=o.k ORDER BY o.id'
    APIS.each do |api|
      full = compare(database, sql, [], api: api)
      (1..full[-2] + 1).each do |limit|
        %i[max_cost lifetime_max_cost].each do |setting|
          compare(database, sql, [], api: api, setting: setting, limit: limit)
        end
      end
      [1, 256, 65_536].each do |limit|
        compare(database, sql, [], api: api, setting: :max_query_memory_bytes, limit: limit, collect: true)
      end
    end
  end

  def snapshots(database)
    sql = 'SELECT o.id,r.v FROM jb_fixed_o o LEFT JOIN jb_fixed_r r ON r.id=o.k WHERE o.id>=$1 ORDER BY o.id'
    sessions, statements = pair(database, sql)
    writer = database.session
    2.times { compare_current(sessions, statements, sql, [1], :query_prepared) }
    first = sessions.each_with_index.map { |current, index| current.query_prepared(statements[index], 1) }
    second = sessions.each_with_index.map { |current, index| current.query_prepared(statements[index], 2) }
    assert((first + second).all? { |cursor| cursor.cost.zero? }, 'bound setup charged before first cursor pull')
    writer.execute('UPDATE jb_fixed_r SET v=200 WHERE id=2')
    current = compare_current(sessions, statements, sql, [1], :query_prepared)
    assert(current[1].first == [1, 200], 'fresh probe retained an earlier table publication')
    assert(first.map(&:rows).uniq == [[[1, 20], [2, 50], [3, nil], [4, nil]]] &&
      second.map(&:rows).uniq == [[[2, 50], [3, nil], [4, nil]]], 'interleaved cursors shared bound values')
    early = sessions.each_with_index.map { |session, index| session.query_prepared(statements[index], 1) }
    assert(early.map { |cursor| cursor.each.first } == [[1, 200], [1, 200]], 'early-close probes changed their first row')
    early.each(&:close)
    writer.execute('ANALYZE jb_fixed_r')
    compare_current(sessions, statements, sql, [1], :query_prepared)
    compare_current(sessions, statements, sql, [nil], :query_prepared)
    assert(database.instance_variable_get(:@pins).empty?, 'bound preparation retained a cursor pin')
  ensure
    [*first.to_a, *second.to_a, *early.to_a, *sessions.to_a, writer].each { |value| value&.close }
  end

  def run
    [false, true].each do |disk|
      Dir.mktmpdir('jed-join-bounds-') do |directory|
        database = disk ? JedNative.create(File.join(directory, 'fixture.jed'), locking: :exclusive, work_mem: 1024) : JedNative.memory
        setup(database)
        query_equivalence(database)
        repeated_and_singleton(database, disk: disk)
        budget_prefixes(database)
        internal_probes(database)
        source_boundaries(database)
        structural_bounds(database)
        snapshots(database)
      ensure
        database&.close
      end
    end
    retention
    puts 'ruby-native prepared join bounds: execution-local structure, fresh probes, fallback, costs and ownership passed'
  end
end

NativePreparedJoinBoundTests.run
