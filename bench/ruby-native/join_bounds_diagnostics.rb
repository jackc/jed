#!/usr/bin/env ruby
# frozen_string_literal: true

# Untimed public-API checks. Physical planning and current bound evaluation are
# compared independently from the execution-local structural preparation.
require 'json'
require 'digest'
require 'optparse'
require 'rbconfig'
require 'fileutils'
require 'tmpdir'
require_relative 'source'
require_relative 'join_functions_diagnostics'

module JoinBoundsDiagnostics
  ROOT = File.expand_path('../..', __dir__)
  PLAN = File.join(__dir__, 'compare/join-functions-plan.json')
  APIS = %i[query_prepared query execute_prepared execute].freeze
  MANY_BINDINGS = [1, 32, 32, 8, 0, nil, 32].freeze
  SINGLE_BINDINGS = [0, 0, 17, 31, 1000, nil, 0].freeze
  STATES = %w[cold changed warm changed_again absent null warm_again].freeze
  CUSTOM_JOBS = [
    { name: 'primary_many', access: 'primary', multiple: true,
      sql: 'SELECT a.id,b.score FROM items a LEFT JOIN items b ON b.id=a.id WHERE a.id<$1 ORDER BY a.id' },
    { name: 'secondary_many', access: 'secondary', multiple: true,
      sql: 'SELECT a.id,b.id,b.label FROM bound_probes a LEFT JOIN bound_lookup b ON b.join_key=a.join_key WHERE a.id<$1 ORDER BY a.id,b.id' },
    { name: 'composite_many', access: 'secondary', multiple: true,
      sql: 'SELECT a.id,b.id,b.label FROM bound_probes a LEFT JOIN bound_composite b ON b.grp=a.grp AND b.join_key=a.join_key WHERE a.id<$1 ORDER BY a.id,b.id' },
    { name: 'secondary_singleton', access: 'secondary', multiple: false,
      sql: 'SELECT a.id,b.id,b.label FROM bound_probes a LEFT JOIN bound_lookup b ON b.join_key=a.join_key WHERE a.id=$1 ORDER BY a.id,b.id' },
    { name: 'composite_singleton', access: 'secondary', multiple: false,
      sql: 'SELECT a.id,b.id,b.label FROM bound_probes a LEFT JOIN bound_composite b ON b.grp=a.grp AND b.join_key=a.join_key WHERE a.id=$1 ORDER BY a.id,b.id' }
  ].freeze
  BUDGET_JOBS = %w[primary_many secondary_many composite_many].freeze
  LIMITS = { parts: 32, nodes: 192, units: 4096 }.freeze
  TRACE_METHODS = {
    'JedNative::Engine' => %i[compile],
    'JedNative::IndexScans' => %i[key_comparisons primary_key_scan secondary_scan],
    'JedNative::IndexNestedLoops' => %i[join_empty_bound? join_index_relation
      prepare_join_index_execution prepare_join_bound_structure join_bound_comparisons],
    'JedNative::JoinPlanner' => %i[two_base_join_plan],
    'JedNative::NWayJoins' => %i[nway_join_plan]
  }.freeze
  module_function

  def jobs
    plan = JSON.parse(File.read(PLAN)).fetch('workloads')
    [['three_table_join_range', true], ['indexed_join', false]].map do |name, multiple|
      { name: name, access: 'primary', multiple: multiple,
        sql: plan.find { |job| job['name'] == name }.fetch('sql') }
    end + CUSTOM_JOBS
  end

  def image(value) = JoinFunctionsDiagnostics.image(value)
  def plan_image(value) = JoinFunctionsDiagnostics.plan_image(value)
  def bindings(job) = job.fetch(:multiple) ? MANY_BINDINGS : SINGLE_BINDINGS

  def structure_image(structure)
    comparisons = (structure.comparisons || []).compact + (structure.empty_comparisons || {}).values
    units = structure.terms ? 1 + structure.terms.length : 0
    units += (structure.keys || []).compact.length * 5
    units += comparisons.sum { |list| 2 + list.length * 3 }
    raise 'structural units exceed limit or recount' unless units == structure.units && units <= LIMITS[:units]
    raise 'prepared key count exceeds limit' unless (structure.keys || []).length <= LIMITS[:parts]
    raise 'mutable cached descriptors' unless (!structure.terms || structure.terms.frozen?) &&
      (structure.keys || []).compact.all?(&:frozen?) &&
      comparisons.all? { |list| list.frozen? && list.all?(&:frozen?) }
    { probes: structure.probes, prepared: structure.prepared, units: structure.units,
      measured_units: units, terms: (structure.terms || []).length,
      keys: (structure.keys || []).compact.length, scan_lists: (structure.comparisons || []).compact.length,
      empty_lists: (structure.empty_comparisons || {}).length, frozen_descriptors: true }
  end

  def outcome(session, statement, sql, params, api, trace: false, setting: nil, limit: nil)
    rows, phase, calls, plans, probes = [], 'admission', Hash.new(0), [], []
    structures, comparison_stack = {}, []
    lifetime = session.lifetime_cost
    session.public_send("#{setting}=", setting == :lifetime_max_cost ? lifetime + limit : limit) if setting
    tracker = TracePoint.new(:call, :return) do |event|
      next unless event.path.start_with?(File.join(@source, 'lib/'))
      owner = event.defined_class.to_s
      if event.event == :call
        calls["#{owner}##{event.method_id}"] += 1 if TRACE_METHODS.fetch(owner, []).include?(event.method_id)
        if owner == 'JedNative::IndexNestedLoops' && event.method_id == :join_index_relation
          access = event.binding.local_variable_get(:plan).fetch(:access)
          probes << { kind: access[:kind], name: access[:name], parts: (access[:parts] || []).length }
        elsif owner == 'JedNative::IndexNestedLoops' && event.method_id == :join_bound_comparisons
          cache, slot = event.binding.local_variable_get(:cache), event.binding.local_variable_get(:slot)
          comparison_stack << cache[slot]
        end
      elsif event.event == :return
        if owner == 'JedNative::PreparedJoins' && %i[nway_join_plan two_base_join_plan].include?(event.method_id)
          plans << plan_image(event.return_value)
        elsif owner == 'JedNative::IndexNestedLoops'
          case event.method_id
          when :prepare_join_index_execution
            returned, original = event.return_value, event.binding.local_variable_get(:plan)
            if (structure = returned[:native_join_bounds])
              raise 'structure was attached to a retained physical plan' if original.key?(:native_join_bounds) ||
                original[:access].key?(:native_join_bounds) || returned.equal?(original) ||
                returned[:access].equal?(original[:access])
              raise 'execution structure was not fresh' unless structure.probes.zero? && !structure.prepared &&
                structure.equal?(returned[:access][:native_join_bounds])
              calls['fresh_execution_structures'] += 1
              structures[structure.object_id] = structure_image(structure)
            end
          when :prepare_join_bound_structure
            if event.return_value
              structure = event.binding.local_variable_get(:structure)
              raise 'structure admitted before the second probe' unless structure.probes == 2 && structure.prepared
              calls['successful_structure_preparations'] += 1
            end
          when :join_bound_comparisons
            saved = comparison_stack.pop
            if saved
              raise 'comparison hit returned a different descriptor' unless saved.equal?(event.return_value)
              calls['saved_comparison_hits'] += 1
            end
          when :join_index_relation
            structure = event.binding.local_variable_get(:plan)[:native_join_bounds]
            structures[structure.object_id] = structure_image(structure) if structure
          end
        end
      end
    end if trace
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
    @last_trace = { calls: calls, physical_plans: plans.uniq, probes: probes,
      structures: structures.values } if trace
    result&.close
    session.max_cost = session.lifetime_max_cost = 0
  end

  def pair(database, sql)
    sessions = [database.session, database.session]
    sessions.last.engine.instance_variable_set(:@native_join_bounds_disabled, true)
    [sessions, sessions.map { |session| session.prepare(sql) }]
  end

  def compare(sessions, statements, sql, params, api, **options)
    actual = outcome(sessions.first, statements.first, sql, params, api, **options)
    enabled = @last_trace if options[:trace]
    expected = outcome(sessions.last, statements.last, sql, params, api, **options)
    disabled = @last_trace if options[:trace]
    raise "bound preparation changed #{api} #{sql} #{params.inspect}" unless actual == expected
    if options[:trace]
      raise 'current physical winner changed' unless enabled[:physical_plans] == disabled[:physical_plans]
      raise 'selected index probes changed' unless enabled[:probes] == disabled[:probes]
      raise 'disabled control prepared a structure' unless disabled[:structures].empty? &&
        disabled[:calls].fetch('successful_structure_preparations', 0).zero? &&
        disabled[:calls].fetch('saved_comparison_hits', 0).zero?
      @last_pair_trace = { enabled: enabled, disabled: disabled }
    end
    actual
  end

  def fixture(database)
    session = database.session
    session.execute('CREATE TABLE items(id i32 PRIMARY KEY, bucket i32, score i32, label text)')
    session.execute("INSERT INTO items VALUES #{32.times.map { |i| "(#{i},#{i % 10},#{i * 37},'row#{i}')" }.join(',')}")
    session.execute('CREATE TABLE bound_probes(id i32 PRIMARY KEY, join_key i32, grp i32)')
    session.execute("INSERT INTO bound_probes VALUES #{32.times.map { |i| "(#{i},#{i == 30 ? 'NULL' : i == 31 ? 1000 : i},#{i % 4})" }.join(',')}")
    %w[bound_lookup bound_composite].each do |table|
      session.execute("CREATE TABLE #{table}(id i32 PRIMARY KEY, join_key i32, grp i32, label text)")
      session.execute("INSERT INTO #{table} VALUES #{128.times.map { |i| "(#{i},#{i / 4},#{i % 4},'value#{i}')" }.join(',')}")
    end
    session.execute('CREATE INDEX bound_lookup_key ON bound_lookup(join_key)')
    session.execute('CREATE INDEX bound_composite_key ON bound_composite(grp,join_key)')
  ensure
    session&.close
  end

  def workload_cases(database, selected)
    sessions = nil
    cases, observations = [], []
    selected.each do |job|
      APIS.each do |api|
        sessions, statements = pair(database, job.fetch(:sql))
        values, traces = [], []
        bindings(job).zip(STATES).each do |parameter, state|
          value = compare(sessions, statements, job.fetch(:sql), [parameter], api, trace: true)
          raise 'ordinary query failed' unless value[:status] == 'rows'
          values << { state: state, params: [parameter], outcome: value }
          traces << { state: state, params: [parameter], trace: @last_pair_trace }
        end
        if @available && job[:multiple]
          reused = traces.any? do |value|
            enabled, disabled = value[:trace].values_at(:enabled, :disabled)
            enabled[:calls].fetch('successful_structure_preparations', 0).positive? &&
              enabled[:calls].fetch('saved_comparison_hits', 0).positive? &&
              enabled[:calls].fetch('JedNative::IndexScans#key_comparisons', 0) <
                disabled[:calls].fetch('JedNative::IndexScans#key_comparisons', 0) &&
              enabled[:probes].count { |probe| probe[:kind].to_s == job[:access] } > 1
          end
          raise "repeated probes did not reuse structure: #{job[:name]} #{api}" unless reused
        elsif @available
          raise 'singleton query prepared repeated-probe structure' unless traces.all? do |value|
            value[:trace][:enabled][:calls].fetch('successful_structure_preparations', 0).zero?
          end
        else
          raise 'baseline prepared a new structure' unless traces.all? { |value| value[:trace][:enabled][:structures].empty? }
        end
        cases << { workload: job[:name], api: api, values: values }
        observations << { workload: job[:name], api: api, values: traces }
        sessions.each(&:close)
        sessions = nil
      end
    end
    [cases, observations]
  ensure
    sessions&.each(&:close)
  end

  def budget_cases(database, selected)
    sessions = nil
    cases = []
    selected.select { |job| BUDGET_JOBS.include?(job[:name]) }.each do |job|
      APIS.each do |api|
        sessions, statements = pair(database, job.fetch(:sql))
        2.times { compare(sessions, statements, job[:sql], [8], api) }
        ordinary = compare(sessions, statements, job[:sql], [8], api)
        cost = ordinary.fetch(:cost)
        limits = [1, cost / 2, cost - 1, cost, cost + 1].select(&:positive?).uniq
        results = %i[max_cost lifetime_max_cost].flat_map do |setting|
          limits.map do |limit|
            value = compare(sessions, statements, job[:sql], [8], api, setting: setting, limit: limit)
            if limit < cost
              expected = setting == :max_cost ? '54P01' : '54P02'
              raise 'below-cost limit did not stop execution' unless value[:status] == 'error' && value[:sqlstate] == expected
            elsif limit > cost
              raise 'above-cost limit changed the outcome' unless value == ordinary
            end
            { setting: setting, limit: limit, outcome: value }
          end
        end
        cases << { workload: job[:name], api: api, params: [8], ordinary: ordinary, limits: results }
        sessions.each(&:close)
        sessions = nil
      end
    end
    cases
  ensure
    sessions&.each(&:close)
  end

  def mutation_cases(database)
    setup = database.session
    sql = CUSTOM_JOBS.find { |job| job[:name] == 'secondary_many' }.fetch(:sql)
    sessions, statements = pair(database, sql)
    cases, traces = [], []
    actions = [nil, 'ANALYZE bound_lookup', 'UPDATE bound_lookup SET join_key=7 WHERE id=0',
      'DROP INDEX bound_lookup_key', 'CREATE INDEX bound_lookup_key ON bound_lookup(join_key)']
    actions.each do |action|
      setup.execute(action) if action
      [1, 8, 32, nil].each do |parameter|
        result = compare(sessions, statements, sql, [parameter], :execute, trace: true)
        observed = @last_pair_trace
        explain = sessions.map { |session| outcome(session, nil, "EXPLAIN #{sql}", [parameter], :execute) }
        raise 'mutation EXPLAIN changed' unless explain.first == explain.last
        cases << { after: action, params: [parameter], outcome: result, explain: explain.first }
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
    abort 'Specify report path and JIT' unless ARGV.empty? && options[:report] && options[:jit]
    raise 'report exists; preserve prior evidence' if File.exist?(options[:report])
    @source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(ROOT, 'impl/ruby-native')))
    before = NativeRubySource.digest(@source)
    require File.join(@source, 'lib/jed_native')
    @available = JedNative::IndexNestedLoops.const_defined?(:JoinBoundStructure, false)
    if @available
      constants = { parts: :MAX_JOIN_BOUND_PARTS, nodes: :MAX_JOIN_BOUND_NODES, units: :MAX_JOIN_BOUND_UNITS }
      raise 'bound structure limits changed' unless constants.transform_values { |name| JedNative::IndexNestedLoops.const_get(name) } == LIMITS
    end
    jit = { yjit: !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?), zjit: !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?) }
    raise 'requested JIT unavailable' unless jit == { yjit: options[:jit] == 'yjit', zjit: options[:jit] == 'zjit' }
    selected, semantic, observations, storage = jobs, {}, {}, {}
    workspace = File.join(ROOT, '.dev/ruby-native-join-bounds-diagnostics')
    FileUtils.mkdir_p(workspace)
    Dir.mktmpdir('bounds-', workspace) do |directory|
      %w[memory disk].each do |mode|
        path = File.join(directory, 'fixture.jed') if mode == 'disk'
        database = mode == 'memory' ? JedNative.memory : JedNative.create(path, locking: :exclusive)
        fixture(database)
        cases, observed = workload_cases(database, selected)
        budgets = budget_cases(database, selected)
        mutations, mutation_observations = mutation_cases(database)
        semantic[mode] = { workloads: cases, budgets: budgets, mutations: mutations }
        observations[mode] = { workloads: observed, mutations: mutation_observations }
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
    report = { schema: 1, mode: 'untimed_join_bounds_diagnostic', runtime: RUBY_DESCRIPTION,
      ruby_executable: RbConfig.ruby, ruby_executable_sha256: Digest::SHA256.file(RbConfig.ruby).hexdigest,
      jit: jit, source: @source, source_sha256: before, final_source_sha256: after,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest, plan_path: PLAN, plan_sha256: Digest::SHA256.file(PLAN).hexdigest,
      bound_preparation_available: @available, limits: LIMITS,
      fixture_rows: { items: 32, probes: 32, lookup: 128, composite: 128 },
      jobs: selected, semantic: semantic, semantic_sha256: Digest::SHA256.hexdigest(JSON.generate(semantic)),
      observations: observations, storage: storage,
      notes: ['No elapsed-time samples; all four cursor/eager APIs drain externally.',
        'Complete values, metadata, costs, errors and consumed prefixes equal the execution-local preparation-disabled control.',
        'Matching binding/state cases retain identical current physical plans; changed bindings need not select different plans.',
        'The private indexed fixture is separate from the unchanged 1000-row main benchmark.',
        'Actual saved comparison-list identity hits and reduced original key_comparisons calls prove repeated-probe reuse.',
        'Every observed structure starts at probe zero on private plan/access copies; preparation waits until probe two.',
        'Descriptor units are independently recounted; bound values and encoded endpoint keys remain fresh per probe.',
        'Index drops, recreation, analyzed statistics and writes are checked without retaining bound values across executions.'] }
    File.write(options[:report], JSON.generate(report) + "\n")
    puts "JOIN bounds #{options[:jit]}: #{semantic.values.sum { |item| item[:workloads].length }} memory/file API groups passed"
  ensure
    database&.close
  end
end

JoinBoundsDiagnostics.main if $PROGRAM_NAME == __FILE__
