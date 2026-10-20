#!/usr/bin/env ruby
# frozen_string_literal: true

# Untimed source-paired planner checks. Allocation-site proof is separate from
# ordinary API tracing and never supplies an elapsed-time measurement.
require 'json'
require 'objspace'
require 'digest'
require 'optparse'
require 'rbconfig'
require 'fileutils'
require 'tmpdir'
require_relative 'source'
require_relative 'join_functions_diagnostics'

module NwayFrontierDiagnostics
  ROOT = File.expand_path('../..', __dir__)
  PLAN = File.join(__dir__, 'compare/nway-frontier-plan.json')
  APIS = %i[query_prepared query execute_prepared execute].freeze
  POLICIES = %w[ordinary physical_disabled].freeze
  STATES = %w[cold warm changed empty_or_absent null warm_again].freeze
  TARGETS = %w[three_table_join three_table_join_range three_table_computed_key
    four_table_join three_table_left_fence five_table_bounded].freeze
  ALLOCATION_JOBS = %w[three_table_join_range three_table_left_fence five_table_bounded].freeze
  TRACE_METHODS = {
    'JedNative::NWayJoins' => %i[nway_join_plan nway_island nway_extensions
      nway_retain nway_strictly_dominated? nway_step_cost],
    'JedNative::JoinPlanner' => %i[estimate_join_rows],
    'JedNative::QueryPlans' => %i[expression_work],
    'JedNative::Engine' => %i[compile]
  }.freeze
  module_function

  def jobs
    selected = %w[scalar indexed_join] + TARGETS
    JSON.parse(File.read(PLAN)).fetch('workloads').select { |job| selected.include?(job['name']) }
      .map { |job| { name: job.fetch('name'), sql: job.fetch('sql') } }
  end

  def bindings(job)
    %w[three_table_join_range five_table_bounded].include?(job[:name]) ? [1, 1, 8, 0, nil, 1] : [0, 0, 17, 1000, nil, 0]
  end

  def image(value) = JoinFunctionsDiagnostics.image(value)

  def structural_image(value)
    case value
    when JedNative::Expression, JedNative::CollationLabel then structural_image(value.to_h)
    when JedNative::CollatedText then { text: value.to_s, collation: structural_image(value.collation_label) }
    when Array then value.map { |child| structural_image(child) }
    when Hash then value.to_h { |key, child| [key, structural_image(child)] }
    when NilClass, TrueClass, FalseClass, Symbol, Integer, Float, String then value
    else raise "unexpected planner descriptor #{value.class}"
    end
  end

  def access_image(access)
    structural_image(access&.slice(:kind, :name, :scan_cost, :scan_rows, :output_rows,
      :logical_rows, :pipeline_cost, :order, :parts, :arms, :terms))
  end

  def bound_image(bound)
    return unless bound
    { access: access_image(bound[:access]), predicate: structural_image(bound[:predicate]),
      filter: structural_image(bound[:filter]), columns: structural_image(bound[:columns]) }
  end

  def plan_image(plan)
    return unless plan.is_a?(Hash)
    return JoinFunctionsDiagnostics.plan_image(plan).merge(index_bounds: bound_image(plan[:index])) unless plan.key?(:order)
    structural_image(plan.slice(:order, :mask, :ordered, :target, :rows, :logical, :cost,
      :output_rows, :join_cost)).merge(
      accesses: (plan[:accesses] || []).map { |access| access_image(access) },
      steps: (plan[:steps] || []).map do |step|
        structural_image(step.slice(:algorithm, :kind, :keys, :ons, :gates, :work, :input_rows,
          :rows, :logical, :pairs, :prefix_cost, :cost, :calls)).merge(
          access: access_image(step[:access]), index_bounds: bound_image(step[:index]))
      end)
  end

  def plan_signature(plan)
    { normalized_sha256: Digest::SHA256.hexdigest(JSON.generate(plan_image(plan))),
      summary: JoinFunctionsDiagnostics.plan_image(plan) }
  end
  def dimensions(value) = %i[cost rows logical].map { |key| value.fetch(key) }
  def strict?(prior, candidate)
    prior.zip(candidate).all? { |left, right| left <= right } &&
      prior.zip(candidate).any? { |left, right| left < right }
  end

  def outcome(session, statement, sql, params, api, trace: false, allocations: false, setting: nil, limit: nil)
    rows, phase, calls, plans, frontiers, decisions = [], 'admission', Hash.new(0), [], [], []
    dominance_frontiers = {}
    constructed, allocation_counts = 0, Hash.new(0)
    lifetime = session.lifetime_cost
    session.public_send("#{setting}=", setting == :lifetime_max_cost ? lifetime + limit : limit) if setting
    tracker = TracePoint.new(:call, :return) do |event|
      next unless event.path.start_with?(File.join(@source, 'lib/'))
      owner = event.defined_class.to_s
      if event.event == :call
        calls["#{owner}##{event.method_id}"] += 1 if TRACE_METHODS.fetch(owner, []).include?(event.method_id)
      elsif owner == 'JedNative::PreparedJoins' && %i[nway_join_plan two_base_join_plan].include?(event.method_id)
        plans << plan_signature(event.return_value)
      elsif owner == 'JedNative::NWayJoins'
        case event.method_id
        when :nway_island
          frontier = event.binding.local_variable_get(:frontier)
          buckets = (frontier || {}).sort_by(&:first).map do |key, states|
            { key: key, states: states.map { |state| plan_image(state) } }
          end
          frontiers << { ordinals: event.binding.local_variable_get(:ordinals),
            final: event.binding.local_variable_get(:final),
            buckets: buckets.length, states: buckets.sum { |bucket| bucket[:states].length },
            full_image_sha256: Digest::SHA256.hexdigest(JSON.generate(buckets)),
            winner: plan_signature(event.return_value) }
        when :nway_extensions
          constructed += event.return_value.length
        when :nway_strictly_dominated?
          values = %i[cost rows logical].map { |name| event.binding.local_variable_get(name) }
          prior = event.binding.local_variable_get(:states).map { |state| dimensions(state) }
          expected = prior.any? { |candidate| strict?(candidate, values) }
          raise 'dominance helper differs from independent strict comparison' unless event.return_value == expected
          calls['strict_construction_skips'] += 1 if event.return_value
          prior_hash = Digest::SHA256.hexdigest(JSON.generate(prior))
          dominance_frontiers[prior_hash] = prior
          decisions << { dimensions: values, prior_dimensions_sha256: prior_hash, skipped: event.return_value,
            exact_dimension_tie: prior.include?(values) }
        end
      end
    end if trace || allocations
    if allocations
      GC.start
      ObjectSpace.trace_object_allocations_clear
      was_disabled = GC.disable
      ObjectSpace.trace_object_allocations_start
    end
    tracker&.enable
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
    if allocations
      ObjectSpace.trace_object_allocations_stop
      ObjectSpace.each_object do |object|
        next unless ObjectSpace.allocation_sourcefile(object) == @nway_file &&
          ObjectSpace.allocation_sourceline(object) == @candidate_line
        allocation_counts[object.class.name] += 1
      end
      ObjectSpace.trace_object_allocations_clear
      GC.enable unless was_disabled
      raise 'candidate-site allocation count differs from actual constructed vectors' unless
        allocation_counts['Array'] == constructed * 6 && allocation_counts['Hash'] == constructed
    end
    @last_trace = { calls: calls, physical_plans: plans, frontiers: frontiers,
      constructed_candidates: constructed, decisions: decisions,
      dominance_frontiers: dominance_frontiers,
      candidate_site_allocations: allocations ? allocation_counts : nil } if trace || allocations
    result&.close
    session.max_cost = session.lifetime_max_cost = 0
  end

  def pair(database, sql, policy)
    sessions = [database.session, database.session]
    sessions.last.engine.instance_variable_set(:@native_nway_frontier_disabled, true)
    if policy == 'physical_disabled'
      sessions.each { |session| session.engine.instance_variable_set(:@native_join_physical_disabled, true) }
    end
    [sessions, sessions.map { |session| session.prepare(sql) }]
  end

  def compare(sessions, statements, sql, params, api, **options)
    actual = outcome(sessions.first, statements.first, sql, params, api, **options)
    enabled = @last_trace if options[:trace] || options[:allocations]
    expected = outcome(sessions.last, statements.last, sql, params, api, **options)
    disabled = @last_trace if enabled
    raise "frontier filtering changed outcome #{api} #{params.inspect}" unless actual == expected
    if enabled
      raise 'current retained frontier or physical winner changed' unless
        enabled.values_at(:frontiers, :physical_plans) == disabled.values_at(:frontiers, :physical_plans)
      skipped = enabled[:calls].fetch('strict_construction_skips', 0)
      raise 'candidate construction difference does not equal actual early skips' unless
        disabled[:constructed_candidates] - enabled[:constructed_candidates] == skipped
      raise 'disabled planner performed early dominance' unless disabled[:decisions].empty?
      %w[JedNative::NWayJoins#nway_step_cost JedNative::JoinPlanner#estimate_join_rows
         JedNative::QueryPlans#expression_work JedNative::Engine#compile].each do |key|
        raise "required estimation work changed: #{key}" unless enabled[:calls][key] == disabled[:calls][key]
      end
      @last_pair_trace = { enabled: enabled, disabled: disabled }
    end
    actual
  end

  def fixture(database)
    session = database.session
    session.execute('CREATE TABLE items(id i32 PRIMARY KEY, bucket i32, score i32, label text)')
    session.execute("INSERT INTO items VALUES #{32.times.map { |i| "(#{i},#{i % 10},#{i * 37},'row#{i}')" }.join(',')}")
  ensure
    session&.close
  end

  def workload_cases(database, selected)
    sessions = nil
    cases, observations = [], []
    selected.each do |job|
      APIS.product(POLICIES).each do |api, policy|
        sessions, statements = pair(database, job[:sql], policy)
        values, traces = [], []
        bindings(job).zip(STATES).each do |parameter, state|
          value = compare(sessions, statements, job[:sql], [parameter], api, trace: true)
          raise 'ordinary query failed' unless value[:status] == 'rows'
          values << { state: state, params: [parameter], outcome: value }
          traces << { state: state, params: [parameter], trace: @last_pair_trace }
        end
        if @available && TARGETS.include?(job[:name]) && policy == 'physical_disabled'
          raise "no actual strict candidate skips #{job[:name]} #{api}" unless traces.any? do |value|
            value[:trace][:enabled][:calls].fetch('strict_construction_skips', 0).positive?
          end
        end
        cases << { workload: job[:name], api: api, policy: policy, values: values }
        observations << { workload: job[:name], api: api, policy: policy, values: traces }
        sessions.each(&:close)
        sessions = nil
      end
    end
    [cases, observations]
  ensure
    sessions&.each(&:close)
  end

  def allocation_cases(database, selected)
    sessions = nil
    selected.select { |job| ALLOCATION_JOBS.include?(job[:name]) }.map do |job|
      sessions, statements = pair(database, job[:sql], 'physical_disabled')
      2.times { compare(sessions, statements, job[:sql], [1], :execute_prepared) }
      value = compare(sessions, statements, job[:sql], [1], :execute_prepared, allocations: true)
      observed = @last_pair_trace
      skipped = observed[:enabled][:calls].fetch('strict_construction_skips', 0)
      raise 'allocation fixture did not exercise early filtering' if @available && skipped.zero?
      sessions.each(&:close)
      sessions = nil
      { workload: job[:name], api: :execute_prepared, policy: 'physical_disabled', params: [1],
        outcome: value, trace: observed }
    end
  ensure
    sessions&.each(&:close)
  end

  def budget_cases(database, selected)
    sessions = nil
    selected.select { |job| ALLOCATION_JOBS.include?(job[:name]) }.flat_map do |job|
      APIS.map do |api|
        sessions, statements = pair(database, job[:sql], 'physical_disabled')
        2.times { compare(sessions, statements, job[:sql], [3], api) }
        ordinary = compare(sessions, statements, job[:sql], [3], api)
        cost = ordinary.fetch(:cost)
        limits = [1, cost / 2, cost - 1, cost, cost + 1].select(&:positive?).uniq
        values = %i[max_cost lifetime_max_cost].flat_map do |setting|
          limits.map do |limit|
            value = compare(sessions, statements, job[:sql], [3], api, setting: setting, limit: limit)
            if limit < cost
              raise 'below-cost limit did not stop execution' unless value[:status] == 'error' &&
                value[:sqlstate] == (setting == :max_cost ? '54P01' : '54P02')
            elsif limit > cost
              raise 'above-cost limit changed ordinary outcome' unless value == ordinary
            end
            { setting: setting, limit: limit, outcome: value }
          end
        end
        sessions.each(&:close)
        sessions = nil
        { workload: job[:name], api: api, params: [3], ordinary: ordinary, limits: values }
      end
    end
  ensure
    sessions&.each(&:close)
  end

  def mutation_cases(database, selected)
    setup = database.session
    job = selected.find { |item| item[:name] == 'three_table_join_range' }
    sessions, statements = pair(database, job[:sql], 'physical_disabled')
    cases, traces = [], []
    actions = [nil, 'ANALYZE items', 'UPDATE items SET score=9 WHERE id=0',
      'CREATE INDEX frontier_bucket ON items(bucket)', 'DROP INDEX frontier_bucket']
    actions.each do |action|
      setup.execute(action) if action
      [1, 8, nil].each do |parameter|
        value = compare(sessions, statements, job[:sql], [parameter], :execute, trace: true)
        observed = @last_pair_trace
        explanations = sessions.map { |session| outcome(session, nil, "EXPLAIN #{job[:sql]}", [parameter], :execute) }
        raise 'current EXPLAIN differs after mutation' unless explanations.first == explanations.last
        cases << { after: action, params: [parameter], outcome: value, explain: explanations.first }
        traces << { after: action, params: [parameter], trace: observed }
      end
    end
    [cases, traces]
  ensure
    sessions&.each(&:close)
    setup&.close
  end

  def main
    database = nil
    options = {}
    OptionParser.new do |parser|
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
      parser.on('--jit NAME', %w[none yjit zjit]) { |value| options[:jit] = value }
    end.parse!
    abort 'Specify a fresh report path and JIT' unless ARGV.empty? && options[:report] && options[:jit] && !File.exist?(options[:report])
    @source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(ROOT, 'impl/ruby-native')))
    before = NativeRubySource.digest(@source)
    require File.join(@source, 'lib/jed_native')
    @available = JedNative::NWayJoins.private_instance_methods(false).include?(:nway_strictly_dominated?)
    @nway_file = File.join(@source, 'lib/jed_native/nway_joins.rb')
    @candidate_line = File.readlines(@nway_file).find_index { |line| line.include?('{ order: state[:order] + [ordinal]') }&.+(1)
    raise 'candidate construction site missing' unless @candidate_line
    jit = { yjit: !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?), zjit: !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?) }
    raise 'requested JIT unavailable' unless jit == { yjit: options[:jit] == 'yjit', zjit: options[:jit] == 'zjit' }
    selected, semantic, observations, storage = jobs, {}, {}, {}
    workspace = File.join(ROOT, '.dev/ruby-native-nway-frontier-diagnostics')
    FileUtils.mkdir_p(workspace)
    Dir.mktmpdir('frontier-', workspace) do |directory|
      %w[memory disk].each do |mode|
        path = File.join(directory, 'fixture.jed') if mode == 'disk'
        database = mode == 'memory' ? JedNative.memory : JedNative.create(path, locking: :exclusive)
        fixture(database)
        cases, observed = workload_cases(database, selected)
        allocations = allocation_cases(database, selected)
        budgets = budget_cases(database, selected)
        mutations, mutation_observations = mutation_cases(database, selected)
        semantic[mode] = { workloads: cases, budgets: budgets, mutations: mutations,
          allocations: allocations.map { |item| item.reject { |key, _| key == :trace } } }
        observations[mode] = { workloads: observed, mutations: mutation_observations, allocations: allocations }
        storage[mode] = mode == 'memory' ? { kind: 'memory' } : {
          kind: 'file', locking: 'exclusive', path: path, bytes: File.size(path),
          sha256: Digest::SHA256.file(path).hexdigest, filesystem_device: File.stat(path).dev }
        database.close
      end
    end
    storage['disk'][:removed_after_run] = !File.exist?(storage.fetch('disk').fetch(:path))
    raise 'private fixture remains' unless storage['disk'][:removed_after_run]
    after = NativeRubySource.digest(@source)
    raise 'source changed during diagnostics' unless before == after
    report = { schema: 1, mode: 'untimed_nway_frontier_diagnostic', runtime: RUBY_DESCRIPTION,
      ruby_executable: RbConfig.ruby, ruby_executable_sha256: Digest::SHA256.file(RbConfig.ruby).hexdigest,
      jit: jit, source: @source, source_sha256: before, final_source_sha256: after,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest, plan_path: PLAN, plan_sha256: Digest::SHA256.file(PLAN).hexdigest,
      frontier_filter_available: @available, fixture_rows: 32, jobs: selected,
      candidate_site: { path: 'lib/jed_native/nway_joins.rb', line: @candidate_line },
      semantic: semantic, semantic_sha256: Digest::SHA256.hexdigest(JSON.generate(semantic)),
      observations: observations, storage: storage,
      notes: ['Untimed exact source-paired outcomes across four fully drained APIs and real memory/file databases.',
        'Ordinary controls keep prior reuse tiers; separate physical-disabled cases force fresh search without disabling resolution reuse.',
        'Every retained frontier state is normalized with choice/estimate fields, keys, ON/gate trees, access parts and bound expressions, then canonically hashed in deterministic order; live table/engine owners are excluded.',
        'Actual true dominance returns independently satisfy strict cost/rows/logical dominance; exact ties retain original handling.',
        'Constructed-candidate differences must equal skipped constructions while estimator/work/step-cost call counts remain identical; focused synthetic tests separately verify callback order.',
        'Separate ObjectSpace cases verify actual six Array allocations and one candidate Hash per constructed state; no allocation trace timings are reported.',
        'Complete cost ceilings compare exact errors and consumed prefixes; exact-ceiling behavior is not assumed uniform across APIs.',
        'Changed bindings, writes, ANALYZE and index changes still select current plans; this optimization retains no new cross-execution state.'] }
    File.write(options[:report], JSON.generate(report) + "\n")
    puts "N-way frontier #{options[:jit]}: #{semantic.values.sum { |item| item[:workloads].length }} memory/file API/policy groups passed"
  ensure
    database&.close
  end
end

NwayFrontierDiagnostics.main if $PROGRAM_NAME == __FILE__
