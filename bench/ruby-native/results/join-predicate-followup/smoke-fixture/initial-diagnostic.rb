#!/usr/bin/env ruby
# frozen_string_literal: true

# Untimed source-paired JOIN predicate checks. Runtime flags and calls to actual
# acquired kernels prove execution; whole-query ABBA establishes performance.
require 'json'
require 'digest'
require 'optparse'
require 'rbconfig'
require 'fileutils'
require 'tmpdir'
require 'weakref'
require_relative 'source'

module JoinPredicateDiagnostics
  ROOT = File.expand_path('../..', __dir__)
  PLAN = File.join(__dir__, 'compare/join-predicate-plan.json')
  APIS = %i[query_prepared query execute_prepared execute].freeze
  STATES = %w[cold warm changed changed_again outside_fixture null warm_again].freeze
  TARGETS = %w[three_table_join_range join_residual].freeze
  BUDGET_JOBS = %w[three_table_join_range three_table_computed_key three_table_left_fence join_residual].freeze
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

  def kernel_image(kernel)
    { source_sha256: Digest::SHA256.hexdigest(kernel.source), source_bytes: kernel.source_bytes,
      node_count: kernel.node_count, pool: image(kernel.pool),
      pool_bytes: kernel.pool.sum { |value| value.is_a?(String) ? value.bytesize : 0 } }
  end

  def tracker(calls, plans = nil)
    acquired = {}
    TracePoint.new(:call, :return) do |event|
      owner, method = event.defined_class.to_s, event.method_id
      if event.path.start_with?(File.join(@source, 'lib/'))
        if event.event == :call && %i[evaluate native_compiled_join_predicate join spill_join spill_index_join
            nway_join_plan two_base_join_plan append].include?(method)
          calls["#{owner}##{method}"] += 1
        elsif event.event == :return && method == :native_compiled_join_predicate
          kernel = event.return_value
          if kernel
            acquired[kernel.object_id] = kernel
            calls['join_kernels_acquired'] += 1
          end
        elsif plans && event.event == :return && owner == 'JedNative::PreparedJoins' &&
            %i[nway_join_plan two_base_join_plan].include?(method)
          plans << plan_image(event.return_value)
        end
      elsif event.event == :call && method == :call && acquired.key?(event.self.object_id)
        calls['join_kernel_calls'] += 1
      end
    end
  end

  def cache_state(engine)
    cache = engine.instance_variable_get(:@native_compiled_join_predicates) || {}
    limit = @available ? JedNative::CompiledPlans::MAX_JOIN_PREDICATE_CACHE_ENTRIES : 64
    raise 'JOIN cache exceeded bound' unless cache.length <= limit
    entries = cache.map do |key, kernel|
      raise 'mutable/non-scalar JOIN source/pool key' unless key.frozen? && key.length == 2 &&
        key[0].frozen? && key[1].frozen? && key[1].all? { |value|
          value.frozen? && [NilClass, TrueClass, FalseClass, Integer, String].any? { |type| value.instance_of?(type) }
        }
      raise 'JOIN kernel retained an execution object' if kernel &&
        kernel.instance_variables.sort != %i[@node_count @pool @source @source_bytes].sort
      raise 'JOIN kernel/key mismatch' if kernel && (kernel.source != key[0] || kernel.pool != key[1] || !kernel.frozen?)
      raise 'JOIN kernel node allowance exceeded' if kernel && kernel.node_count > JedNative::CompiledExpression::MAX_NODES
      raise 'JOIN pool/source exceeded compiler limits' unless key[0].bytesize <= JedNative::CompiledExpression::MAX_SOURCE_BYTES &&
        key[1].length <= JedNative::CompiledExpression::MAX_POOL &&
        key[1].sum { |value| value.is_a?(String) ? value.bytesize : 0 } <= JedNative::CompiledExpression::MAX_POOL_BYTES
      { source_sha256: Digest::SHA256.hexdigest(key[0]), pool_sha256: Digest::SHA256.hexdigest(JSON.generate(image(key[1]))),
        materialized: !kernel.nil?, source_bytes: key[0].bytesize, pool_entries: key[1].length,
        pool_bytes: key[1].sum { |value| value.is_a?(String) ? value.bytesize : 0 }, node_count: kernel&.node_count }
    end
    { entries: entries, retained: cache.length, kernels: cache.values.compact.length, limit: limit }
  end

  def pair(database, sql)
    sessions = [database.session, database.session]
    sessions.last.engine.instance_variable_set(:@native_join_compilation_disabled, true)
    [sessions, sessions.map { |session| session.prepare(sql) }]
  end

  def compare(sessions, statements, sql, params, api, **options)
    actual = outcome(sessions.first, statements.first, sql, params, api, **options)
    enabled = @last_trace if options[:trace]
    expected = outcome(sessions.last, statements.last, sql, params, api, **options)
    disabled = @last_trace if options[:trace]
    raise "JOIN compilation changed #{api} #{sql} #{params.inspect}" unless actual == expected
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
          raise 'ordinary JOIN workload failed' unless answer[:status] == 'rows'
          observed << { state: STATES[index], params: [value], trace: @last_pair_trace }
          { state: STATES[index], params: [value], outcome: answer }
        end
        if TARGETS.include?(job['name']) && @available
          reduced = observed.any? do |value|
            value[:trace][:enabled][:calls].fetch('join_kernel_calls', 0).positive?
          end
          raise "compiled JOIN predicate not exercised #{job['name']} #{api}" unless reduced
        end
        cases << { workload: job['name'], api: api, values: values }
        observations << { workload: job['name'], api: api, values: observed,
          cache: { enabled: cache_state(sessions.first.engine), disabled: cache_state(sessions.last.engine) } }
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

  COLUMNS = [{ name: 'a', type: 'i32' }, { name: 'b', type: 'i32' },
    { name: 'flag', type: 'boolean' }, { name: 'label', type: 'text' }].freeze
  HELPER_SQL = ["a = b AND abs(a - b) >= $1::i32", "flag AND a + b > $1::i32",
    "label >= 'row5' AND a > $1::i32"].freeze

  def helper_outcome(engine, expression, kernel, row, params, setting: nil, limit: nil)
    engine.instance_variable_set(:@params, params)
    engine.instance_variable_set(:@cost, 0)
    engine.instance_variable_set(:@lifetime_cost, 0)
    engine.instance_variable_set(:@scalar_bytes, 0)
    engine.public_send("#{setting}=", limit) if setting
    value = kernel ? kernel.call(engine, row, params) : engine.evaluate(expression, row)
    { status: 'value', value: image(value), cost: engine.cost, lifetime_cost: engine.lifetime_cost }
  rescue JedNative::Error => error
    { status: 'error', sqlstate: error.sqlstate, message: error.message,
      cost: engine.cost, lifetime_cost: engine.lifetime_cost }
  ensure
    engine.max_cost = engine.lifetime_max_cost = 0
  end

  def resolve(engine, sql)
    engine.compile_filter(JedNative::Parser.new(sql).parse_expression, COLUMNS)
  end

  def helper_cases
    engine = JedNative::Engine.new
    rows = [[0, 0, true, 'row5'], [2, 1, false, 'row4'], [nil, 1, nil, nil],
      [1, nil, true, 'é'], [2_147_483_647, 1, true, 'row6']]
    HELPER_SQL.map do |sql|
      expression = resolve(engine, sql)
      kernel = nil
      if @available
        raise 'JOIN first use did not defer' unless engine.send(:native_compiled_join_predicate, expression).nil?
        kernel = engine.send(:native_compiled_join_predicate, expression)
        raise 'JOIN second use failed' unless kernel
        raise 'JOIN equal program not reused' unless engine.send(:native_compiled_join_predicate, resolve(engine, sql)).equal?(kernel)
      end
      cases = rows.product([-1, 0, 1, nil]).map do |row, param|
        params = [param]
        ordinary = helper_outcome(engine, expression, nil, row, params)
        actual = helper_outcome(engine, expression, kernel, row, params)
        raise 'JOIN helper outcome differs' unless ordinary == actual
        boundaries = (1..ordinary[:cost] + 1).flat_map do |limit|
          %i[max_cost lifetime_max_cost].map do |setting|
            left = helper_outcome(engine, expression, nil, row, params, setting: setting, limit: limit)
            right = helper_outcome(engine, expression, kernel, row, params, setting: setting, limit: limit)
            raise 'JOIN helper cost prefix differs' unless left == right
            { setting: setting, limit: limit, outcome: right }
          end
        end
        { row: image(row), params: params, outcome: actual, boundaries: boundaries }
      end
      { sql: sql, cases: cases }
    end
  end

  def cache_proof
    return { available: false } unless @available
    engine = JedNative::Engine.new
    expression = resolve(engine, 'a > 7')
    raise 'first JOIN admission was eager' unless engine.send(:native_compiled_join_predicate, expression).nil?
    kernel = engine.send(:native_compiled_join_predicate, expression)
    raise 'second JOIN admission failed' unless kernel
    engine.instance_variable_set(:@native_join_compilation_disabled, true)
    raise 'JOIN-only disable ignored' if engine.send(:native_compiled_join_predicate, expression)
    engine.instance_variable_set(:@native_join_compilation_disabled, false)
    raise 'JOIN cache changed while disabled' unless engine.send(:native_compiled_join_predicate, expression).equal?(kernel)
    expression.args.last.value = 8
    raise 'changed literal reused old pool' unless engine.send(:native_compiled_join_predicate, expression).nil?
    changed = engine.send(:native_compiled_join_predicate, expression)
    raise 'changed literal did not get separate kernel' unless changed && !changed.equal?(kernel) && changed.pool != kernel.pool
    engine.instance_variable_set(:@native_compilation_disabled, true)
    raise 'global disable ignored' if engine.send(:native_compiled_join_predicate, expression)
    engine.instance_variable_set(:@native_compilation_disabled, false)
    engine.instance_variable_set(:@native_compilation_immediate, true)
    raise 'immediate mode deferred' unless engine.send(:native_compiled_join_predicate, resolve(engine, 'a > 9'))
    before = cache_state(engine)
    workers = Thread.new do
      local = JedNative::Engine.new
      weak = []
      96.times do |index|
        resolved = resolve(local, "a > #{index}")
        raise 'pool churn first use did not defer' unless local.send(:native_compiled_join_predicate, resolved).nil?
        current = local.send(:native_compiled_join_predicate, resolved)
        raise 'pool churn second use failed' unless current
        weak << WeakRef.new(current)
        expected = helper_outcome(local, resolved, nil, [50, 0, true, 'row5'], [])
        raise 'pool churn semantics changed' unless expected == helper_outcome(local, resolved, current, [50, 0, true, 'row5'], [])
      end
      state = cache_state(local)
      raise 'JOIN LRU did not cap materialized kernels' unless state[:retained] == 64 && state[:kernels] == 64
      [state, weak]
    end
    during, weak = workers.value
    workers = nil
    after = 3.times.map do
      GC.start(full_mark: true, immediate_sweep: true)
      weak.count { |item| item.weakref_alive? }
    end
    { available: true, second_use: true, equal_program_reused: true, fresh_pool_after_mutation: true,
      disabled_cache_unchanged: true, global_disabled: true, immediate_materialized: true,
      admission_cache: before, variants: 96, during: during, live_kernels_after_completed_thread_and_gc: after,
      note: 'Weak kernel counts are observations; the existing bounded generated-method cache and VM code may retain classes independently.' }
  end

  def main
    database = nil
    original_tmpdir = ENV['TMPDIR']
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
    @available = JedNative::CompiledPlans.const_defined?(:MAX_JOIN_PREDICATE_CACHE_ENTRIES)
    jit = { yjit: !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?), zjit: !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?) }
    raise 'requested JIT unavailable' unless jit == { yjit: options[:jit] == 'yjit', zjit: options[:jit] == 'zjit' }
    selected, semantic, observations, storage = jobs, {}, {}, {}
    workspace = File.join(ROOT, '.dev/ruby-native-join-predicate-diagnostics')
    FileUtils.mkdir_p(workspace)
    Dir.mktmpdir('work-', workspace) do |directory|
      ENV['TMPDIR'] = directory
      %w[memory disk].each do |mode|
        path = File.join(directory, 'fixture.jed') if mode == 'disk'
        database = mode == 'memory' ? JedNative.memory : JedNative.create(path, locking: :exclusive)
        database.work_mem = 1024 if mode == 'disk'
        fixture(database)
        cases, observed = workload_cases(database, selected)
        cases[:budgets] = budget_cases(database, selected)
        cases[:mutations], mutation_observations = mutation_cases(database)
        semantic[mode] = cases
        observations[mode] = { workloads: observed, mutations: mutation_observations }
        storage[mode] = mode == 'memory' ? { kind: 'memory' } : {
          kind: 'file', locking: 'exclusive', path: path, bytes: File.size(path),
          sha256: Digest::SHA256.file(path).hexdigest, filesystem_device: File.stat(path).dev,
          scratch_directory: directory }
        database.close
      end
    end
    ENV['TMPDIR'] = original_tmpdir
    storage['disk'][:removed_after_run] = !File.exist?(storage.fetch('disk').fetch(:path))
    raise 'private fixture remains' unless storage['disk'][:removed_after_run]
    semantic[:helpers] = helper_cases
    cache = cache_proof
    after = NativeRubySource.digest(@source)
    raise 'source changed during diagnostics' unless before == after
    report = { schema: 1, mode: 'untimed_join_predicate_diagnostic', runtime: RUBY_DESCRIPTION,
      ruby_executable: RbConfig.ruby, ruby_executable_sha256: Digest::SHA256.file(RbConfig.ruby).hexdigest,
      jit: jit, source: @source, source_sha256: before, final_source_sha256: after,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest, plan_path: PLAN, plan_sha256: Digest::SHA256.file(PLAN).hexdigest,
      join_compilation_available: @available, fixture_rows: 32, jobs: selected, disk_work_mem: 1024,
      compiler_limits: { nodes: JedNative::CompiledExpression::MAX_NODES, depth: JedNative::CompiledExpression::MAX_DEPTH,
        source_bytes: JedNative::CompiledExpression::MAX_SOURCE_BYTES, pool_entries: JedNative::CompiledExpression::MAX_POOL,
        pool_bytes: JedNative::CompiledExpression::MAX_POOL_BYTES, join_cache_entries: 64,
        minimum_join_candidates: @available ? JedNative::CompiledPlans::MIN_JOIN_PREDICATE_CANDIDATES : nil },
      cache_proof: cache,
      semantic: semantic, semantic_sha256: Digest::SHA256.hexdigest(JSON.generate(semantic)),
      observations: observations, storage: storage,
      notes: ['Untimed diagnostics; call counts are not CPU-time percentages.',
        'Four APIs drain externally and compare complete outcomes, metadata, costs, errors and consumed prefixes against the original JOIN evaluator.',
        'Private 32-row fixture is separate from the unchanged 1000-row main benchmark.',
        'Matching binding/state cases have identical physical plans and EXPLAIN across source revisions; different bindings may choose different plans.',
        'Each ON root preserves current values and exact error/cost prefixes. Runtime JIT flags and calls to actual helper-acquired kernels prove the new execution path; no new method-specific JIT experiment is claimed.'] }
    File.write(options[:report], JSON.generate(report) + "\n")
    puts "Join predicate #{options[:jit]}: #{selected.length * APIS.length * 2} memory/file API groups passed"
  ensure
    ENV['TMPDIR'] = original_tmpdir
    database&.close
  end
end

JoinPredicateDiagnostics.main if $PROGRAM_NAME == __FILE__
