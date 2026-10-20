#!/usr/bin/env ruby
# frozen_string_literal: true

# Untimed source-paired estimator checks. No elapsed-time samples or retained
# estimates: current values, costs, plans and helper boundaries are compared.
require 'json'
require 'digest'
require 'optparse'
require 'rbconfig'
require 'fileutils'
require 'tmpdir'
require_relative 'source'

module WorkEstimationDiagnostics
  ROOT = File.expand_path('../..', __dir__)
  PLAN = File.join(__dir__, 'compare/multiway-plan.json')
  APIS = %i[query_prepared query execute_prepared execute].freeze
  STATES = %w[cold warm changed changed_again outside_fixture null warm_again].freeze
  ORIGINAL_WORK_LAYERS = %w[
    JedNative::DeepStack JedNative::HostFunctions JedNative::ExplainSubqueries
    JedNative::SubqueryAccess JedNative::AccessPlanning JedNative::QueryPlans
  ].freeze
  TARGETS = %w[three_table_join_range three_table_left_fence].freeze
  BUDGET_JOBS = %w[three_table_join_range three_table_computed_key three_table_left_fence].freeze
  module_function

  def jobs
    JSON.parse(File.read(PLAN)).fetch('workloads').map { |job| job.slice('name', 'sql') }
  end

  def bindings = [0, 0, 17, 3, 1000, nil, 0]

  def image(value)
    case value
    when Array then value.map { |item| image(item) }
    when JedNative::CollatedText
      { ruby_class: value.class.name, text: value.to_s,
        collation: value.collation_label&.to_h&.transform_values { |item| item.respond_to?(:sort_key) ? item.class.name : item } }
    when String then { ruby_class: value.class.name, text: value }
    else value
    end
  end

  def plan_image(plan)
    return unless plan.is_a?(Hash)
    access = ->(value) { value&.slice(:kind, :name, :scan_cost, :scan_rows, :output_rows, :logical_rows, :pipeline_cost) }
    if plan.key?(:order)
      plan.slice(:order, :mask, :ordered, :target, :rows, :logical, :cost, :output_rows, :join_cost).merge(
        accesses: (plan[:accesses] || []).map { |value| access.call(value) },
        steps: (plan[:steps] || []).map do |step|
          step.slice(:algorithm, :kind, :work, :input_rows, :rows, :logical, :pairs, :prefix_cost, :cost, :calls).merge(
            access: access.call(step[:access]), index: access.call(step.dig(:index, :access)))
        end)
    else
      plan.slice(:algorithm, :reverse, :ordered, :target, :rows, :join_rows, :output_count, :cost).merge(
        scans: (plan[:scans] || []).map { |value| access.call(value) },
        index: access.call(plan.dig(:index, :access)))
    end
  end

  def outcome(session, statement, sql, params, api, trace: false, setting: nil, limit: nil)
    rows, phase, calls, plans = [], 'admission', Hash.new(0), []
    tracker = tracker(calls, plans) if trace
    tracker&.enable
    lifetime = session.lifetime_cost
    session.public_send("#{setting}=", setting == :lifetime_max_cost ? lifetime + limit : limit) if setting
    result = api.to_s.end_with?('_prepared') ? session.public_send(api, statement, *params) : session.public_send(api, sql, *params)
    admission_columns, admission_types = result.columns.dup, result.column_types.dup
    phase = 'pull'
    result.each { |row| rows << image(row.to_a) }
    { status: 'rows', rows: rows, columns: result.columns, types: result.column_types,
      admission_columns: admission_columns, admission_types: admission_types,
      cost: result.cost, lifetime_cost: session.lifetime_cost - lifetime }
  rescue JedNative::Error => error
    { status: 'error', phase: phase, sqlstate: error.sqlstate, message: error.message,
      rows: rows, admission_columns: admission_columns, admission_types: admission_types,
      cost: result&.cost || session.engine.cost, lifetime_cost: session.lifetime_cost - lifetime }
  ensure
    tracker&.disable
    @last_trace = { calls: calls, physical_plans: plans.uniq } if trace
    result&.close
    session.max_cost = session.lifetime_max_cost = 0
  end

  def fixture(database)
    session = database.session
    session.execute('CREATE TABLE items(id i32 PRIMARY KEY, bucket i32, score i32, label text)')
    values = 32.times.map { |i| "(#{i},#{i % 10},#{i * 37},'row#{i}')" }.join(',')
    session.execute("INSERT INTO items VALUES #{values}")
  ensure
    session&.close
  end

  def tracker(calls, plans = nil)
    TracePoint.new(:call, :return) do |event|
      next unless event.path.start_with?(File.join(@source, 'lib/'))
      owner = event.defined_class.to_s
      if event.event == :call && (event.method_id == :expression_work ||
          %i[nway_join_plan two_base_join_plan estimate_predicate].include?(event.method_id))
        calls["#{owner}##{event.method_id}"] += 1
      elsif plans && event.event == :return && owner == 'JedNative::PreparedJoins' &&
          %i[nway_join_plan two_base_join_plan].include?(event.method_id)
        plans << plan_image(event.return_value)
      end
    end
  end

  def pair(database, sql)
    sessions = [database.session, database.session]
    sessions.last.engine.instance_variable_set(:@native_planner_work_disabled, true)
    [sessions, sessions.map { |session| session.prepare(sql) }]
  end

  def compare(sessions, statements, sql, params, api, **options)
    actual = outcome(sessions.first, statements.first, sql, params, api, **options)
    enabled = @last_trace if options[:trace]
    expected = outcome(sessions.last, statements.last, sql, params, api, **options)
    disabled = @last_trace if options[:trace]
    raise "estimator changed #{api} #{sql} #{params.inspect}" unless actual == expected
    if options[:trace]
      raise 'current physical winner changed' unless enabled[:physical_plans] == disabled[:physical_plans]
      @last_pair_trace = { enabled: enabled, disabled: disabled }
    end
    actual
  end

  def workload_cases(database, selected)
    sessions = nil
    cases, observations, explains = [], [], []
    selected.each do |job|
      sql = job.fetch('sql')
      APIS.each do |api|
        sessions, statements = pair(database, sql)
        observed = []
        values = bindings.each_with_index.map do |value, index|
          answer = compare(sessions, statements, sql, [value], api, trace: true)
          raise 'ordinary estimator workload failed' unless answer[:status] == 'rows'
          observed << { state: STATES[index], params: [value], trace: @last_pair_trace }
          { state: STATES[index], params: [value], outcome: answer }
        end
        if TARGETS.include?(job['name']) && @available
          reduced = observed.any? do |value|
            value[:trace][:enabled][:calls].fetch('JedNative::QueryPlans#expression_work', 0) <
              value[:trace][:disabled][:calls].fetch('JedNative::QueryPlans#expression_work', 0)
          end
          raise "fast scalar work path not exercised #{job['name']} #{api}" unless reduced
        end
        cases << { workload: job['name'], api: api, values: values }
        observations << { workload: job['name'], api: api, values: observed }
        sessions.each(&:close)
      end
      explain = "EXPLAIN (VERBOSE) #{sql}"
      sessions, statements = pair(database, explain)
      answer = compare(sessions, statements, explain, [17], :query_prepared)
      raise 'ordinary EXPLAIN failed' unless answer[:status] == 'rows'
      explains << { workload: job['name'], params: [17], outcome: answer }
      sessions.each(&:close)
    end
    [{ workloads: cases, explains: explains }, observations]
  ensure
    sessions&.each(&:close)
  end

  def budget_cases(database, selected)
    sessions = nil
    BUDGET_JOBS.flat_map do |name|
      sql = selected.find { |job| job['name'] == name }.fetch('sql')
      APIS.map do |api|
        sessions, statements = pair(database, sql)
        2.times { compare(sessions, statements, sql, [3], api) }
        ordinary = compare(sessions, statements, sql, [3], api)
        raise 'ordinary budget query failed' unless ordinary[:status] == 'rows'
        cost = ordinary.fetch(:cost)
        values = [1, [cost / 2, 1].max, [cost - 1, 1].max, cost, cost + 1].uniq.sort.flat_map do |limit|
          %i[max_cost lifetime_max_cost].map do |setting|
            { setting: setting, limit: limit,
              outcome: compare(sessions, statements, sql, [3], api, setting: setting, limit: limit) }
          end
        end
        sessions.each(&:close)
        { workload: name, api: api, params: [3], ordinary: ordinary, values: values }
      end
    end
  ensure
    sessions&.each(&:close)
  end

  def mutation_cases(database)
    writer = database.session
    writer.execute_script('CREATE TABLE oa(id i32 PRIMARY KEY,k i32); CREATE TABLE ob(id i32 PRIMARY KEY,bucket i32,score i32); CREATE TABLE oc(id i32 PRIMARY KEY,label text); CREATE INDEX ob_bucket ON ob(bucket)')
    writer.execute('INSERT INTO oa VALUES(1,0),(2,13),(3,999)')
    writer.execute("INSERT INTO ob VALUES #{(1..16).map { |i| "(#{i},#{i <= 12 ? 0 : i},#{i * 10})" }.join(',')}")
    writer.execute("INSERT INTO oc VALUES #{(1..16).map { |i| "(#{i},'right#{i}')" }.join(',')}")
    sql = 'SELECT a.id,b.id,c.label FROM oa a LEFT JOIN ob b ON b.bucket=a.k AND b.score >= $1 LEFT JOIN oc c ON c.id=b.id AND c.id<>3 WHERE a.id=$2 ORDER BY a.id,b.id'
    sessions, statements = pair(database, sql)
    explanations = sessions.map { |session| session.prepare("EXPLAIN (VERBOSE) #{sql}") }
    values, observations = [], []
    [nil, 'ANALYZE ob', 'UPDATE ob SET score=200 WHERE id=1', 'CREATE INDEX ob_score ON ob(score)', 'DROP INDEX ob_score'].each do |action|
      writer.execute(action) if action
      [[0, 1], [130, 1], [0, 2], [0, 3], [nil, 1], [0, 1]].each do |params|
        answer = compare(sessions, statements, sql, params, :query_prepared, trace: true)
        trace = @last_pair_trace
        raise 'ordinary mutation query failed' unless answer[:status] == 'rows'
        explain = compare(sessions, explanations, "EXPLAIN (VERBOSE) #{sql}", params, :query_prepared)
        raise 'mutation EXPLAIN failed' unless explain[:status] == 'rows'
        values << { after: action, params: params, outcome: answer, explain: explain }
        observations << { after: action, params: params, trace: trace }
      end
    end
    [values, observations]
  ensure
    sessions&.each(&:close)
    writer&.close
  end

  def helper_outcome(engine, expression)
    before = engine.cost
    calls = Hash.new(0)
    trace = tracker(calls)
    trace.enable
    value = engine.send(:expression_work, expression)
    { status: 'value', value: value }
  rescue StandardError => error
    { status: 'error', class: error.class.name, message: error.message }
  ensure
    trace&.disable
    raise 'work estimation changed metered cost' unless before == engine.cost
    raise 'estimator stack context escaped invocation' unless engine.instance_variable_get(:@native_expression_work_depth).to_i.zero?
    @helper_trace = calls
  end

  def helper_cases
    database = JedNative.memory
    sessions = [database.session, database.session]
    engines = sessions.map(&:engine)
    engines.last.instance_variable_set(:@native_planner_work_disabled, true)
    leaf = ->(value = 1) { JedNative::Expression.new(kind: :literal, type: 'i32', value: value) }
    unary = ->(child) { JedNative::Expression.new(kind: :unary, type: 'i32', value: '-', args: [child]) }
    binary = ->(left, right) { JedNative::Expression.new(kind: :binary, type: 'i32', value: '+', args: [left, right]) }
    subclass = Class.new(JedNative::Expression)
    array_class = Class.new(Array)
    bomb_class = Class.new(JedNative::Expression) do
      def kind = raise(ArgumentError, 'diagnostic estimator failure')
    end
    plan_class = Struct.new(:cost)
    host_class = Struct.new(:cost, :name)
    semantic, observations = [], []
    run = lambda do |name, expressions, expected, positive: false, fallback: nil|
      answers, traces = engines.zip(expressions).map do |engine, expression|
        answer = helper_outcome(engine, expression)
        [answer, @helper_trace]
      end.transpose
      raise "helper result changed #{name}" unless answers[0] == answers[1] && answers.first == expected
      if @available && positive
        raise "helper scalar admission missing #{name}" unless traces.first.fetch('JedNative::QueryPlans#expression_work', 0) <
          traces.last.fetch('JedNative::QueryPlans#expression_work', 0)
      end
      if fallback
        raise "helper fallback bypassed #{name}" unless traces.first.fetch("#{fallback}#expression_work", 0).positive?
      end
      semantic << { name: name, outcome: answers.first, cost_unchanged: true, depth_restored: true }
      observations << { name: name, positive: positive, fallback_owner: fallback, calls: { enabled: traces.first, disabled: traces.last } }
    end
    value = ->(number) { { status: 'value', value: number } }
    run.call('scalar_tree', 2.times.map { binary.call(unary.call(leaf.call), JedNative::Expression.new(kind: :cast, type: 'i64', args: [leaf.call])) }, value.call(3), positive: true)
    run.call('nested_arrays', 2.times.map { [leaf.call, [unary.call(leaf.call), binary.call(leaf.call, leaf.call)]] }, value.call(2), positive: true, fallback: 'JedNative::QueryPlans')
    run.call('zero_unit_with_child', 2.times.map { JedNative::Expression.new(kind: :column, type: 'i32', value: 0, args: [unary.call(leaf.call)]) }, value.call(1), positive: true)
    run.call('deep_scalar', 2.times.map { 40.times.reduce(leaf.call) { |child, _| unary.call(child) } }, value.call(40), positive: true)
    mutable = 2.times.map { binary.call(leaf.call, leaf.call) }
    run.call('mutation_initial', mutable, value.call(1), positive: true)
    mutable.each { |expression| expression.args << unary.call(leaf.call) }
    run.call('mutation_args', mutable, value.call(2), positive: true)
    mutable.each { |expression| expression.kind = :numeric_context }
    run.call('mutation_kind', mutable, value.call(2), positive: true)
    mutable.each { |expression| expression.extra = { native_subplan: plan_class.new(7), correlated: true } }
    run.call('mutation_subplan', mutable, value.call(9), positive: true)
    mutable.each { |expression| expression.extra[:native_subplan].cost = 13 }
    run.call('mutation_subplan_cost', mutable, value.call(15), positive: true)
    mutable.each { |expression| expression.extra = nil }
    run.call('mutation_extra_removed', mutable, value.call(2), positive: true)
    mutable.each { |expression| expression.extra = {} }
    run.call('mutation_empty_extra', mutable, value.call(2), positive: true)
    quantified = 2.times.map do
      JedNative::Expression.new(kind: :quantified, type: 'boolean', args: [leaf.call,
        JedNative::Expression.new(kind: :literal, type: 'i32[]', value: [1, nil, 3])])
    end
    run.call('quantified_literal', quantified, value.call(4), fallback: 'JedNative::AccessPlanning')
    quantified.each { |expression| expression.args[1].value << 4 }
    run.call('quantified_literal_mutated', quantified, value.call(5), fallback: 'JedNative::AccessPlanning')
    quantified.each { |expression| expression.args[1] = JedNative::Expression.new(kind: :param, type: 'i32[]', value: 0) }
    [[[1, 2]], [[]], [nil]].zip([3, 1, 1]).each_with_index do |(params, cost), index|
      engines.each { |engine| engine.instance_variable_set(:@params, params.map { |item| item.is_a?(Array) ? item.dup : item }) }
      run.call("quantified_binding_#{index}", quantified, value.call(cost), fallback: 'JedNative::AccessPlanning')
    end
    memberships = 2.times.map { JedNative::Expression.new(kind: :query_membership, type: 'boolean', args: [leaf.call], extra: { correlated: false, rows: [[1], [2], [nil]] }) }
    [false, true, false].each_with_index do |context, index|
      engines.each { |engine| engine.instance_variable_set(:@native_estimate_membership, context) }
      memberships.each { |expression| expression.extra[:rows] << [4] } if index == 2
      run.call("membership_context_#{index}", memberships, value.call([4, 1, 5][index]), fallback: 'JedNative::SubqueryAccess')
    end
    hosts = 2.times.map { JedNative::Expression.new(kind: :host_call, type: 'i32', value: host_class.new(9, 'diagnostic_cost'), args: [binary.call(leaf.call, leaf.call)]) }
    run.call('host_cost', hosts, value.call(10), fallback: 'JedNative::HostFunctions')
    hosts.each { |expression| expression.value.cost = 17 }
    run.call('host_cost_mutated', hosts, value.call(18), fallback: 'JedNative::HostFunctions')
    run.call('aggregate_short_circuit', 2.times.map { JedNative::Expression.new(kind: :call, type: 'i64', value: 'sum', args: [bomb_class.new]) }, value.call(0), fallback: 'JedNative::QueryPlans')
    run.call('expression_subclass', 2.times.map { subclass.new(kind: :binary, type: 'i32', value: '+', args: [leaf.call, leaf.call]) }, value.call(1), fallback: 'JedNative::QueryPlans')
    run.call('args_subclass', 2.times.map { JedNative::Expression.new(kind: :binary, type: 'i32', value: '+', args: array_class.new([leaf.call, unary.call(leaf.call)])) }, value.call(2), fallback: 'JedNative::QueryPlans')
    run.call('deep_exception', 2.times.map { 40.times.reduce(bomb_class.new) { |child, _| unary.call(child) } },
      { status: 'error', class: 'ArgumentError', message: 'diagnostic estimator failure' })
    run.call('after_exception', 2.times.map { binary.call(leaf.call, leaf.call) }, value.call(1), positive: true)
    [semantic, observations]
  ensure
    sessions&.each(&:close)
    database&.close
  end

  def main
    database = nil
    options = {}
    OptionParser.new do |parser|
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
      parser.on('--jit NAME', %w[none yjit zjit]) { |value| options[:jit] = value }
    end.parse!
    abort 'Specify report path and JIT' unless ARGV.empty? && options[:report] && options[:jit]
    raise 'report exists; preserve prior evidence' if File.exist?(options[:report])
    @source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(ROOT, 'impl/ruby-native')))
    before = NativeRubySource.digest(@source)
    require File.join(@source, 'lib/jed_native')
    @available = File.read(File.join(@source, 'lib/jed_native/deep_stack.rb')).include?('@native_planner_work_disabled')
    raise 'stack slice changed' unless JedNative::DeepStack::STACK_SLICE == 12
    jit = { yjit: !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?), zjit: !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?) }
    raise 'requested JIT unavailable' unless jit == { yjit: options[:jit] == 'yjit', zjit: options[:jit] == 'zjit' }
    selected, semantic, observations, storage = jobs, {}, {}, {}
    workspace = File.join(ROOT, '.dev/ruby-native-work-estimation-diagnostics')
    FileUtils.mkdir_p(workspace)
    Dir.mktmpdir('work-', workspace) do |directory|
      %w[memory disk].each do |mode|
        path = File.join(directory, 'fixture.jed') if mode == 'disk'
        database = mode == 'memory' ? JedNative.memory : JedNative.create(path, locking: :exclusive)
        fixture(database)
        cases, observed = workload_cases(database, selected)
        cases[:budgets] = budget_cases(database, selected)
        cases[:mutations], mutation_observations = mutation_cases(database)
        semantic[mode] = cases
        observations[mode] = { workloads: observed, mutations: mutation_observations }
        storage[mode] = mode == 'memory' ? { kind: 'memory' } : {
          kind: 'file', locking: 'exclusive', path: path, bytes: File.size(path),
          sha256: Digest::SHA256.file(path).hexdigest, filesystem_device: File.stat(path).dev }
        database.close
      end
    end
    storage['disk'][:removed_after_run] = !File.exist?(storage.fetch('disk').fetch(:path))
    raise 'private fixture remains' unless storage['disk'][:removed_after_run]
    semantic[:helpers], observations[:helpers] = helper_cases
    after = NativeRubySource.digest(@source)
    raise 'source changed during diagnostics' unless before == after
    report = { schema: 1, mode: 'untimed_work_estimation_diagnostic', runtime: RUBY_DESCRIPTION,
      ruby_executable: RbConfig.ruby, ruby_executable_sha256: Digest::SHA256.file(RbConfig.ruby).hexdigest,
      jit: jit, source: @source, source_sha256: before, final_source_sha256: after,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest, plan_path: PLAN, plan_sha256: Digest::SHA256.file(PLAN).hexdigest,
      optimized_work_available: @available, fixture_rows: 32, jobs: selected, stack_slice: 12,
      semantic: semantic, semantic_sha256: Digest::SHA256.hexdigest(JSON.generate(semantic)),
      observations: observations, storage: storage,
      notes: ['Untimed diagnostics; call counts are not CPU-time percentages.',
        'Four APIs drain externally and compare complete outcomes, metadata, costs, errors and consumed prefixes against the original estimator.',
        'Private 32-row fixture is separate from the unchanged 1000-row main benchmark.',
        'Matching binding/state cases have identical physical plans and EXPLAIN across source revisions; different bindings may choose different plans.',
        'Internal probes preserve live context, expression/metadata mutation, aggregate short-circuit, special fallback and exception recovery; no result is memoized.'] }
    File.write(options[:report], JSON.generate(report) + "\n")
    puts "Work estimation #{options[:jit]}: #{selected.length * APIS.length * 2} memory/file API groups passed"
  ensure
    database&.close
  end
end

WorkEstimationDiagnostics.main if $PROGRAM_NAME == __FILE__
