#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit, untimed multiway join-resolution evidence. Every cursor is drained
# externally. The archived source has only two-table resolution reuse.
require 'json'
require 'digest'
require 'optparse'
require 'rbconfig'
require 'fileutils'
require 'tmpdir'
require 'weakref'
require_relative 'source'

module MultiwayPlanDiagnostics
  ROOT = File.expand_path('../..', __dir__)
  PLAN = File.join(__dir__, 'compare/multiway-plan.json')
  APIS = %i[query_prepared query execute_prepared execute].freeze
  LIMITS = { entries: 64, roots: 128, key_units: 4096, key_bytes: 65_536 }.freeze
  TRACE_METHODS = {
    'JedNative::Engine' => %i[compile compile_filter],
    'JedNative::JoinPlanner' => %i[two_base_join_plan],
    'JedNative::NWayJoins' => %i[nway_join_plan nway_extensions],
    'JedNative::AccessPlanning' => %i[estimate_predicate],
    'JedNative::IndexNestedLoops' => %i[join_index_plan]
  }.freeze
  module_function

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

  def cache(session) = session.engine.instance_variable_get(:@native_join_resolution_plans) || {}

  def snapshot_frozen?(value)
    case value
    when Array then value.frozen? && value.all? { |item| snapshot_frozen?(item) }
    when Hash then value.frozen? && value.all? { |key, item| snapshot_frozen?(key) && snapshot_frozen?(item) }
    when String then value.frozen?
    else true
    end
  end

  def cache_state(session)
    entries = cache(session).values
    state = { entries: entries.length, roots: entries.map { |entry| entry.roots.length },
      key_units: entries.map(&:key_units), key_bytes: entries.map(&:key_bytes),
      column_scopes: entries.map { |entry| entry.columns.length },
      interning_available: JedNative::PreparedJoins::Entry.members.include?(:nodes),
      interned_nodes: entries.map { |entry| entry.respond_to?(:nodes) ? entry.nodes.length : 0 },
      interned_scopes: entries.map { |entry| entry.respond_to?(:scopes) ? entry.scopes.length : 0 },
      snapshots_frozen: entries.map do |entry|
        keys = entry.roots.keys
        keys += entry.nodes.keys + entry.scopes.keys if entry.respond_to?(:nodes)
        keys.all? { |key| snapshot_frozen?(key) }
      end }
    raise 'join cache exceeded statement bound' if state[:entries] > LIMITS.fetch(:entries)
    %i[roots key_units key_bytes].each do |key|
      raise "join cache exceeded #{key} bound" if state.fetch(key).any? { |count| count > LIMITS.fetch(key) }
    end
    raise 'retained snapshot is mutable' unless state[:snapshots_frozen].all?
    %i[interned_nodes interned_scopes].each do |key|
      raise "unbounded #{key}" unless state.fetch(key).zip(state.fetch(:roots)).all? { |count, roots| count <= roots }
    end
    raise 'join frame escaped its invocation' if session.engine.instance_variable_get(:@native_join_resolution_frame)
    state
  end

  def plan_image(plan)
    return unless plan.is_a?(Hash)
    access = lambda do |value|
      value&.slice(:kind, :name, :scan_cost, :scan_rows, :output_rows, :pipeline_cost)
    end
    if plan.key?(:order)
      return plan.slice(:order, :mask, :ordered, :target, :rows, :logical, :cost, :output_rows, :join_cost).merge(
        accesses: (plan[:accesses] || []).map { |value| access.call(value) },
        steps: (plan[:steps] || []).map do |step|
          step.slice(:algorithm, :kind, :work, :input_rows, :rows, :logical, :pairs, :prefix_cost, :cost, :calls).merge(
            access: access.call(step[:access]), index: access.call(step.dig(:index, :access)))
        end)
    end
    plan.slice(:algorithm, :reverse, :ordered, :target, :rows, :join_rows, :output_count, :cost).merge(
      scans: (plan[:scans] || []).map { |value| access.call(value) }, index: access.call(plan.dig(:index, :access)))
  end

  def outcome(session, statement, sql, params, api, setting: nil, limit: 0, trace: false)
    rows, phase, plans, calls = [], 'admission', [], Hash.new(0)
    extension_peak = 0
    lifetime = session.lifetime_cost
    session.public_send("#{setting}=", setting == :lifetime_max_cost ? lifetime + limit : limit) if setting
    tracker = TracePoint.new(:call, :return) do |event|
      next unless event.path.start_with?(File.join(@source, 'lib/'))
      if event.event == :call && TRACE_METHODS.fetch(event.defined_class.to_s, []).include?(event.method_id)
        calls["#{event.defined_class}##{event.method_id}"] += 1
      elsif event.event == :return && ((event.defined_class == JedNative::JoinPlanner && event.method_id == :two_base_join_plan) ||
                                      (event.defined_class == JedNative::NWayJoins && event.method_id == :nway_join_plan))
        plans << plan_image(event.return_value)
      elsif event.event == :return && event.defined_class == JedNative::PreparedJoins && event.method_id == :nway_extension_inputs
        frame = event.self.instance_variable_get(:@native_join_resolution_frame)
        count = frame&.extensions&.length || 0
        raise 'local extension memo exceeded its bound' if count > 128
        extension_peak = [extension_peak, count].max
      end
    end if trace
    tracker&.enable
    result = api.to_s.end_with?('_prepared') ? session.public_send(api, statement, *params) : session.public_send(api, sql, *params)
    phase = 'pull'
    result.each { |row| rows << image(row.to_a) }
    { status: 'rows', rows: rows, columns: result.columns, types: result.column_types,
      cost: result.cost, lifetime_cost: session.lifetime_cost - lifetime }
  rescue JedNative::Error => error
    { status: 'error', phase: phase, sqlstate: error.sqlstate, message: error.message,
      rows: rows, cost: result&.cost || session.engine.cost, lifetime_cost: session.lifetime_cost - lifetime }
  ensure
    tracker&.disable
    @last_trace = { calls: calls, physical_plans: plans.uniq, local_extension_entries_peak: extension_peak } if trace
    result&.close
    session.max_cost = session.lifetime_max_cost = 0
  end

  def pair(database, sql)
    sessions = [database.session, database.session]
    sessions.last.engine.instance_variable_set(:@native_join_resolution_disabled, true)
    [sessions, sessions.map { |session| session.prepare(sql) }]
  end

  def compare(sessions, statements, sql, params, api, **options)
    actual = outcome(sessions[0], statements[0], sql, params, api, **options)
    actual_trace = @last_trace if options[:trace]
    expected = outcome(sessions[1], statements[1], sql, params, api, **options)
    expected_trace = @last_trace if options[:trace]
    raise "join reuse changed #{api} #{sql} #{params.inspect}" unless actual == expected
    if options[:trace]
      raise 'parameter-sensitive physical winner changed' unless actual_trace[:physical_plans] == expected_trace[:physical_plans]
      @last_pair_trace = { enabled: actual_trace, disabled: expected_trace }
    end
    raise 'disabled control retained join resolutions' unless cache(sessions.last).empty?
    actual
  end

  def fixture(database)
    session = database.session
    session.execute('CREATE TABLE items(id i32 PRIMARY KEY, bucket i32, score i32, label text)')
    values = 1000.times.map { |i| "(#{i},#{i % 10},#{i * 37 % 10_000},'row#{i}')" }.join(',')
    session.execute("INSERT INTO items VALUES #{values}")
  ensure
    session&.close
  end

  def workload_cases(database, jobs)
    sessions = nil
    cases, observations = [], []
    jobs.each do |job|
      sql = job.fetch('sql')
      bindings = job.fetch('name') == 'three_table_join_range' ? [0, 1, 10, 90, 100, nil, 1] : [0, 3, 17, 999, 1000, nil, 0]
      APIS.each do |api|
        sessions, statements = pair(database, sql)
        2.times { compare(sessions, statements, sql, [0], api) }
        entry = cache(sessions.first).values.first
        values = bindings.map do |value|
          answer = compare(sessions, statements, sql, [value], api, trace: true)
          observations << { workload: job.fetch('name'), api: api, params: [value],
            cache: cache_state(sessions.first), original_entry_still_cached: entry && cache(sessions.first).values.any? { |item| item.equal?(entry) },
            trace: @last_pair_trace }
          { params: [value], outcome: answer }
        end
        cases << { workload: job.fetch('name'), api: api, values: values }
        sessions.each(&:close)
      end
    end
    [cases, observations]
  ensure
    sessions&.each(&:close)
  end

  def budget_cases(database, jobs)
    sessions = nil
    names = %w[three_table_join three_table_join_range three_table_left_fence]
    names.flat_map do |name|
      sql = jobs.find { |job| job.fetch('name') == name }.fetch('sql')
      APIS.map do |api|
        sessions, statements = pair(database, sql)
        2.times { compare(sessions, statements, sql, [10], api) }
        ordinary = compare(sessions, statements, sql, [10], api)
        cost = ordinary.fetch(:cost)
        ceilings = cost <= 64 ? (1..cost + 1).to_a : [1, 2, 5, 10, cost / 2, cost - 1, cost, cost + 1].uniq.sort
        outcomes = ceilings.flat_map do |limit|
          %i[max_cost lifetime_max_cost].map do |setting|
            { setting: setting, limit: limit,
              outcome: compare(sessions, statements, sql, [10], api, setting: setting, limit: limit) }
          end
        end
        sessions.each(&:close)
        { workload: name, api: api, params: [10], ordinary: ordinary, ceilings: outcomes,
          exhaustive_cost_prefixes: cost <= 64 }
      end
    end
  ensure
    sessions&.each(&:close)
  end

  def skew_cases(database)
    writer = database.session
    writer.execute_script('CREATE TABLE jo(id i32 PRIMARY KEY,k i32); CREATE TABLE js(id i32 PRIMARY KEY,bucket i32); CREATE TABLE jt(id i32 PRIMARY KEY,v i32); CREATE INDEX js_bucket ON js(bucket)')
    writer.execute("INSERT INTO jo VALUES #{(1..12).map { |i| "(#{i},#{i})" }.join(',')}")
    writer.execute("INSERT INTO js VALUES #{(1..40).map { |i| "(#{i},#{i <= 35 ? 0 : i})" }.join(',')}")
    writer.execute("INSERT INTO jt VALUES #{(1..40).map { |i| "(#{i},#{i * 2})" }.join(',')}")
    sql = 'SELECT a.id, b.id, c.v FROM jo a JOIN js b ON a.k = b.id JOIN jt c ON c.id = b.id WHERE b.bucket = $1 ORDER BY a.id LIMIT 10'
    sessions, statements = pair(database, sql)
    explanations = sessions.map { |session| session.prepare("EXPLAIN (VERBOSE) #{sql}") }
    values, observations = [], []
    [nil, 'ANALYZE js', 'UPDATE js SET bucket = 0 WHERE id = 36', 'CREATE INDEX js_id ON js(id)', 'DROP INDEX js_id'].each do |action|
      writer.execute(action) if action
      [0, 36, 90, nil, 0].each do |value|
        answer = compare(sessions, statements, sql, [value], :query_prepared, trace: true)
        trace = @last_pair_trace
        explain = compare(sessions, explanations, "EXPLAIN (VERBOSE) #{sql}", [value], :query_prepared)
        values << { after: action, params: [value], outcome: answer, explain: explain }
        observations << { after: action, params: [value], cache: cache_state(sessions.first), trace: trace }
      end
    end
    [values, observations]
  ensure
    sessions&.each(&:close)
    writer&.close
  end

  def churn
    return { available: false } unless defined?(JedNative::PreparedJoins::MAX_INPUTS)
    result = Thread.new do
      database = JedNative.memory
      current = database.session
      current.execute('CREATE TABLE tiny(id i32 PRIMARY KEY,v i32)')
      current.execute('INSERT INTO tiny VALUES(1,7),(2,11)')
      weak = []
      96.times do |index|
        sql = "SELECT a.id + #{index},b.v,c.v FROM tiny a JOIN tiny b ON b.id=a.id JOIN tiny c ON c.id=b.id WHERE a.id=$1 AND b.id=$1 AND c.id=$1"
        statement = current.prepare(sql)
        2.times do
          value = outcome(current, statement, sql, [1], :query_prepared)
          raise 'churn answer changed' unless value[:status] == 'rows' && value[:rows] == [[index + 1, 7, 7]]
        end
        weak << WeakRef.new(cache(current).values.last)
      end
      state = cache_state(current)
      raise 'churn did not fill bounded cache' unless state[:entries] == 64
      current.close
      database.close
      { available: true, variants: 96, retained_before_close: state, weak: weak }
    end.value
    weak = result.delete(:weak)
    result[:live_entries_after_completed_thread_gc] = 3.times.map do
      GC.start(full_mark: true, immediate_sweep: true)
      weak.count { |reference| reference.weakref_alive? }
    end
    result
  end

  def main
    database = nil
    options = {}
    OptionParser.new do |parser|
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
      parser.on('--jit NAME', %w[none yjit zjit]) { |value| options[:jit] = value }
    end.parse!
    abort 'Specify report path and JIT' unless ARGV.empty? && options[:report] && options[:jit]
    raise 'diagnostic report already exists' if File.exist?(options[:report])
    @source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(ROOT, 'impl/ruby-native')))
    before = NativeRubySource.digest(@source)
    require File.join(@source, 'lib/jed_native')
    jit = { yjit: !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?), zjit: !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?) }
    raise 'requested JIT is unavailable' unless jit == { yjit: options[:jit] == 'yjit', zjit: options[:jit] == 'zjit' }
    jobs = JSON.parse(File.read(PLAN)).fetch('workloads')
    semantic, observations = {}, {}
    storage = {}
    workspace = File.join(ROOT, '.dev/ruby-native-multiway-diagnostics')
    FileUtils.mkdir_p(workspace)
    Dir.mktmpdir('join-', workspace) do |directory|
      %w[memory disk].each do |mode|
        path = File.join(directory, 'fixture.jed') if mode == 'disk'
        database = mode == 'memory' ? JedNative.memory : JedNative.create(path, locking: :exclusive)
        fixture(database)
        values, trace = workload_cases(database, jobs)
        budgets = budget_cases(database, jobs)
        skew, skew_trace = skew_cases(database)
        semantic[mode] = { workloads: values, budgets: budgets, skew: skew }
        observations[mode] = { workloads: trace, skew: skew_trace }
        admitted = %w[indexed_join]
        admitted += %w[three_table_join three_table_join_miss three_table_join_range three_table_computed_key four_table_join] if
          defined?(JedNative::PreparedJoins::MAX_INPUTS)
        trace.each do |row|
          expected = admitted.include?(row[:workload])
          raise "#{mode} cache admission differs for #{row[:workload]}" unless row[:cache][:entries].positive? == expected
          raise "#{mode} did not retain roots for #{row[:workload]}" if expected && row[:cache][:roots].none?(&:positive?)
        end
        storage[mode] = mode == 'memory' ? { kind: 'memory' } : {
          kind: 'file', locking: 'exclusive', path: path, bytes: File.size(path),
          sha256: Digest::SHA256.file(path).hexdigest, filesystem_device: File.stat(path).dev }
        database.close
      end
    end
    storage['disk'][:removed_after_run] = !File.exist?(storage.fetch('disk').fetch(:path))
    raise 'private file diagnostic was not cleaned up' unless storage['disk'][:removed_after_run]
    retention = churn
    after = NativeRubySource.digest(@source)
    raise 'source changed during diagnostics' unless before == after
    report = { mode: 'untimed_multiway_resolution_diagnostic', runtime: RUBY_DESCRIPTION, ruby_executable: RbConfig.ruby,
      ruby_executable_sha256: Digest::SHA256.file(RbConfig.ruby).hexdigest, jit: jit,
      source: @source, source_sha256: before, final_source_sha256: after,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest, plan_sha256: Digest::SHA256.file(PLAN).hexdigest,
      compiler_available: !!defined?(JedNative::PreparedJoins), multiway_available: !!defined?(JedNative::PreparedJoins::MAX_INPUTS),
      limits: LIMITS.merge(input_sources: defined?(JedNative::PreparedJoins::MAX_INPUTS) ? JedNative::PreparedJoins::MAX_INPUTS : 2,
        local_extensions: defined?(JedNative::PreparedJoins::EXTENSION_ENTRIES) ? JedNative::PreparedJoins::EXTENSION_ENTRIES : 0),
      semantic: semantic, semantic_sha256: Digest::SHA256.hexdigest(JSON.generate(semantic)),
      observations: observations, storage: storage, retention: retention,
      notes: ['No timing samples. Every successful/error prefix, metadata field, exact cost and lifetime-cost delta is compared with join resolution reuse disabled.',
        'Physical winners are observed in the original JoinPlanner/NWayJoins methods; duplicate identical per-invocation plans are collapsed for comparison. EXPLAIN remains on its ordinary path.',
        'All budget prefixes are checked for queries costing at most64; larger cases use the explicitly recorded representative ceilings.',
        'Cache retention is observed only after its owning thread and database have completed; remaining weak references are reported without hiding VM roots.'] }
    File.write(options[:report], JSON.pretty_generate(report) + "\n")
    puts "Multiway diagnostic #{options[:jit]}: memory/file APIs, parameters, plans, budgets and cache observations passed"
  ensure
    database&.close
  end
end

MultiwayPlanDiagnostics.main if $PROGRAM_NAME == __FILE__
