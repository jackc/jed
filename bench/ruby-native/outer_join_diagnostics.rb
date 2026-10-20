#!/usr/bin/env ruby
# frozen_string_literal: true

# Untimed source-paired LEFT-resolution checks. Existing inner-join caching
# stays enabled; the comparison session disables all retained join resolution.
require 'json'
require 'digest'
require 'optparse'
require 'rbconfig'
require 'fileutils'
require 'tmpdir'
require 'weakref'
require_relative 'source'

module OuterJoinDiagnostics
  ROOT = File.expand_path('../..', __dir__)
  PLAN = File.join(__dir__, 'compare/multiway-plan.json')
  APIS = %i[query_prepared query execute_prepared execute].freeze
  STATES = %w[cold warm changed null_extension absent null warm_again].freeze
  LIMITS = { entries: 64, roots: 128, key_units: 4096, key_bytes: 65_536,
    columns: 8, sources: 8, source_units: 4096, source_bytes: 65_536 }.freeze
  EXTRA_JOBS = [
    { name: 'left_then_inner', admission: 'outer',
      sql: 'SELECT a.id, b.score, c.label FROM items a LEFT JOIN items b ON b.id=a.id AND b.bucket<>3 JOIN items c ON c.id=b.id WHERE a.id=$1 ORDER BY a.id' },
    { name: 'three_left_chain', admission: 'outer',
      sql: 'SELECT a.id, b.score, c.label FROM items a LEFT JOIN items b ON b.id=a.id AND b.bucket<>3 LEFT JOIN items c ON c.id=b.id AND c.bucket<>5 WHERE a.id=$1 ORDER BY a.id' },
    { name: 'four_left_middle', admission: 'outer',
      sql: 'SELECT a.id, b.score, c.label, d.bucket FROM items a JOIN items b ON b.id=a.id LEFT JOIN items c ON c.id=b.id AND c.bucket<>3 JOIN items d ON d.id=a.id WHERE a.id=$1 AND b.id=$1 AND d.id=$1 ORDER BY a.id' },
    { name: 'four_left_chain', admission: 'outer',
      sql: 'SELECT a.id, b.score, c.label, d.bucket FROM items a LEFT JOIN items b ON b.id=a.id AND b.bucket<>3 LEFT JOIN items c ON c.id=b.id AND c.bucket<>5 LEFT JOIN items d ON d.id=c.id AND d.bucket<>7 WHERE a.id=$1 ORDER BY a.id' },
    { name: 'full_fallback', admission: 'fallback',
      sql: 'SELECT a.id, b.score, c.label FROM items a JOIN items b ON b.id=a.id FULL JOIN items c ON c.id=b.id WHERE a.id=$1 AND b.id=$1 ORDER BY a.id' },
    { name: 'function_fallback', admission: 'fallback',
      sql: 'SELECT abs(a.id), b.score, c.label FROM items a JOIN items b ON b.id=a.id LEFT JOIN items c ON c.id=b.id AND c.bucket<>3 WHERE a.id=$1 AND b.id=$1 ORDER BY a.id' }
  ].freeze
  BUDGET_JOBS = %w[three_table_left_fence three_left_chain four_left_middle].freeze
  TRACE_METHODS = {
    'JedNative::Engine' => %i[compile compile_filter],
    'JedNative::PreparedJoins' => %i[prepared_join_root],
    'JedNative::NWayJoins' => %i[nway_join_plan],
    'JedNative::JoinPlanner' => %i[two_base_join_plan]
  }.freeze
  module_function

  def jobs
    plan = JSON.parse(File.read(PLAN)).fetch('workloads')
    %w[three_table_left_fence three_table_join].map do |name|
      job = plan.find { |item| item['name'] == name }
      { name: name, sql: job.fetch('sql'), admission: name == 'three_table_join' ? 'inner' : 'outer' }
    end + EXTRA_JOBS
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

  def cache(session) = session.engine.instance_variable_get(:@native_join_resolution_plans) || {}

  def snapshot_frozen?(value)
    case value
    when Array then value.frozen? && value.all? { |item| snapshot_frozen?(item) }
    when Hash then value.frozen? && value.all? { |key, item| snapshot_frozen?(key) && snapshot_frozen?(item) }
    when String then value.frozen?
    else true
    end
  end

  def snapshot_size(value)
    units, bytes = 1, value.is_a?(String) ? value.bytesize : 0
    children = case value
               when Hash then value.to_a.flatten(1)
               when Array then value
               else []
               end
    children.each do |child|
      count, size = snapshot_size(child)
      units += count
      bytes += size
    end
    [units, bytes]
  end

  def cache_state(session)
    entries = cache(session).values
    state = { entries: entries.length, physical_entries: entries.count(&:physical),
      per_entry: entries.map do |entry|
        metadata = entry.sources.to_a + (@available ? entry.columns.to_a : [])
        measured_units, measured_bytes = metadata.map { |pair| snapshot_size(pair) }.transpose.map(&:sum)
        measured_units ||= 0
        measured_bytes ||= 0
        { roots: entry.roots.length, key_units: entry.key_units, key_bytes: entry.key_bytes,
          nodes: entry.nodes.length, scopes: entry.scopes.length,
          columns: entry.columns.length, sources: entry.sources.length,
          source_units: entry.source_units, source_bytes: entry.source_bytes,
          measured_source_units: measured_units, measured_source_bytes: measured_bytes,
          column_snapshots_frozen: entry.columns.keys.all? { |key| snapshot_frozen?(key) } &&
            entry.columns.values.all? { |columns| snapshot_frozen?(columns) },
          snapshots_frozen: (entry.roots.keys + entry.nodes.keys + entry.scopes.keys).all? { |key| snapshot_frozen?(key) } &&
            entry.sources.keys.all? { |key| snapshot_frozen?(key) } &&
            entry.sources.values.all? { |columns| snapshot_frozen?(columns) } }
      end }
    raise 'statement cache exceeded its bound' if entries.length > LIMITS[:entries]
    state[:per_entry].each do |item|
      LIMITS.each do |key, limit|
        next if key == :entries
        raise "retained #{key} exceeded its bound" if item.fetch(key) > limit
      end
      raise 'retained snapshot container is mutable' unless item[:snapshots_frozen]
      raise 'retained column descriptor is mutable' if @available && !item[:column_snapshots_frozen]
      raise 'retained metadata accounting differs' unless
        item[:source_units] == item[:measured_source_units] && item[:source_bytes] == item[:measured_source_bytes]
      raise 'interned key count exceeds roots' if item[:nodes] > item[:roots] || item[:scopes] > item[:roots]
    end
    raise 'join frame escaped execution' if session.engine.instance_variable_get(:@native_join_resolution_frame)
    state
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
    rows, phase, calls, plans, frames = [], 'admission', Hash.new(0), [], []
    lifetime = session.lifetime_cost
    session.public_send("#{setting}=", setting == :lifetime_max_cost ? lifetime + limit : limit) if setting
    tracker = TracePoint.new(:call, :return) do |event|
      next unless event.path.start_with?(File.join(@source, 'lib/'))
      owner = event.defined_class.to_s
      if event.event == :call && TRACE_METHODS.fetch(owner, []).include?(event.method_id)
        calls["#{owner}##{event.method_id}"] += 1
      elsif event.event == :return && owner == 'JedNative::PreparedJoins'
        if %i[nway_join_plan two_base_join_plan].include?(event.method_id)
          plans << plan_image(event.return_value)
        elsif event.method_id == :prepared_join_frame && event.return_value
          frame = event.return_value
          frames << (frame.respond_to?(:physical_reuse) ? frame.physical_reuse : 'legacy')
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
    @last_trace = { calls: calls, physical_plans: plans.uniq, physical_reuse_frames: frames } if trace
    result&.close
    session.max_cost = session.lifetime_max_cost = 0
  end

  def pair(database, sql)
    sessions = [database.session, database.session]
    sessions.last.engine.instance_variable_set(:@native_join_resolution_disabled, true)
    [sessions, sessions.map { |session| session.prepare(sql) }]
  end

  def compare(sessions, statements, sql, params, api, **options)
    actual = outcome(sessions.first, statements.first, sql, params, api, **options)
    enabled = @last_trace if options[:trace]
    expected = outcome(sessions.last, statements.last, sql, params, api, **options)
    disabled = @last_trace if options[:trace]
    raise "resolution changed #{api} #{sql} #{params.inspect}" unless actual == expected
    raise 'disabled session retained resolutions' unless cache(sessions.last).empty?
    if options[:trace]
      raise 'current physical winner changed' unless enabled[:physical_plans] == disabled[:physical_plans]
      @last_pair_trace = { enabled: enabled, disabled: disabled }
    end
    actual
  end

  def fixture(database)
    session = database.session
    session.execute('CREATE TABLE items(id i32 PRIMARY KEY, bucket i32, score i32, label text)')
    values = 32.times.map { |i| "(#{i},#{i % 10},#{i * 37},'row#{i}')" }.join(',')
    session.execute("INSERT INTO items VALUES #{values}")
  ensure
    session&.close
  end

  def workload_cases(database, selected)
    sessions = nil
    cases, observations = [], []
    selected.each do |job|
      APIS.each do |api|
        sql = job.fetch(:sql)
        sessions, statements = pair(database, sql)
        original = nil
        observed = []
        values = bindings.each_with_index.map do |value, index|
          answer = compare(sessions, statements, sql, [value], api, trace: true)
          raise 'ordinary LEFT/control query failed' unless answer[:status] == 'rows'
          original ||= cache(sessions.first).values.first
          observed << { state: STATES[index], params: [value], trace: @last_pair_trace,
            cache: cache_state(sessions.first),
            original_entry_still_cached: !!original && cache(sessions.first).values.any? { |entry| entry.equal?(original) } }
          { state: STATES[index], params: [value], outcome: answer }
        end
        admission = job[:admission] == 'inner' || job[:admission] == 'outer' && @available
        if admission
          reused = observed.any? do |value|
            %w[warm warm_again].include?(value[:state]) && value[:original_entry_still_cached] &&
              value[:cache][:per_entry].any? { |entry| entry[:roots].positive? } &&
              value[:trace][:enabled][:calls].fetch('JedNative::Engine#compile', 0) <
                value[:trace][:disabled][:calls].fetch('JedNative::Engine#compile', 0)
          end
          raise "retained roots were not reused: #{job[:name]} #{api}" unless reused
        else
          raise 'unsupported source/shape retained resolutions' unless observed.all? { |value| value[:cache][:entries].zero? }
        end
        if job[:admission] == 'outer'
          observed.each do |value|
            raise 'LEFT retained a physical template' unless value[:cache][:physical_entries].zero?
            next if value[:params].include?(nil) || value[:params] == [1000]
            trace = value[:trace][:enabled]
            raise 'LEFT skipped fresh physical planning' unless trace[:calls].fetch('JedNative::NWayJoins#nway_join_plan', 0).positive?
            if @available
              raise 'LEFT frame permits physical reuse' unless trace[:physical_reuse_frames].include?(false) &&
                trace[:physical_reuse_frames].all? { |flag| flag == false }
            end
          end
        end
        cases << { workload: job[:name], api: api, values: values }
        observations << { workload: job[:name], api: api, values: observed }
        sessions.each(&:close)
      end
    end
    [cases, observations]
  ensure
    sessions&.each(&:close)
  end

  def budget_cases(database, selected)
    sessions = nil
    BUDGET_JOBS.flat_map do |name|
      sql = selected.find { |job| job[:name] == name }.fetch(:sql)
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
        state = cache_state(sessions.first)
        raise 'mutation query retained physical plan' unless state[:physical_entries].zero?
        explain = compare(sessions, explanations, "EXPLAIN (VERBOSE) #{sql}", params, :query_prepared)
        raise 'mutation EXPLAIN failed' unless explain[:status] == 'rows'
        values << { after: action, params: params, outcome: answer, explain: explain }
        observations << { after: action, params: params, cache: state, trace: trace }
      end
    end
    [values, observations]
  ensure
    sessions&.each(&:close)
    writer&.close
  end

  def churn
    result = Thread.new do
      database = JedNative.memory
      session = database.session
      session.execute('CREATE TABLE tiny(id i32 PRIMARY KEY,v i32)')
      session.execute('INSERT INTO tiny VALUES(1,7),(2,11)')
      weak = []
      96.times do |index|
        sql = "SELECT a.id+#{index},b.v,c.v FROM tiny a LEFT JOIN tiny b ON b.id=a.id LEFT JOIN tiny c ON c.id=b.id WHERE a.id=$1"
        statement = session.prepare(sql)
        2.times do
          answer = outcome(session, statement, sql, [1], :query_prepared)
          raise 'churn answer changed' unless answer[:status] == 'rows' && answer[:rows] == [[index + 1, 7, 7]]
        end
        entry = cache(session).values.last
        weak << WeakRef.new(entry) if entry
      end
      state = cache_state(session)
      raise 'churn admission/bound differs' unless state[:entries] == (@available ? 64 : 0) && state[:physical_entries].zero?
      session.close
      database.close
      { variants: 96, retained_before_close: state, weak: weak }
    end.value
    weak = result.delete(:weak)
    result[:live_entries_after_completed_thread_gc] = 3.times.map do
      GC.start(full_mark: true, immediate_sweep: true)
      weak.count(&:weakref_alive?)
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
    raise 'report exists; preserve prior evidence' if File.exist?(options[:report])
    @source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(ROOT, 'impl/ruby-native')))
    before = NativeRubySource.digest(@source)
    require File.join(@source, 'lib/jed_native')
    @available = JedNative::PreparedJoins::Frame.members.include?(:physical_reuse)
    constants = { entries: :CACHE_ENTRIES, roots: :ROOT_ENTRIES, key_units: :KEY_UNITS, key_bytes: :KEY_BYTES,
      columns: :MAX_INPUTS, sources: :MAX_INPUTS, source_units: :KEY_UNITS, source_bytes: :KEY_BYTES }
    raise 'cache limits changed' unless constants.transform_values { |name| JedNative::PreparedJoins.const_get(name) } == LIMITS
    jit = { yjit: !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?), zjit: !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?) }
    raise 'requested JIT unavailable' unless jit == { yjit: options[:jit] == 'yjit', zjit: options[:jit] == 'zjit' }
    selected, semantic, observations, storage = jobs, {}, {}, {}
    workspace = File.join(ROOT, '.dev/ruby-native-outer-join-diagnostics')
    FileUtils.mkdir_p(workspace)
    Dir.mktmpdir('outer-', workspace) do |directory|
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
    retained = churn
    after = NativeRubySource.digest(@source)
    raise 'source changed during diagnostics' unless before == after
    report = { schema: 1, mode: 'untimed_outer_join_diagnostic', runtime: RUBY_DESCRIPTION,
      ruby_executable: RbConfig.ruby, ruby_executable_sha256: Digest::SHA256.file(RbConfig.ruby).hexdigest,
      jit: jit, source: @source, source_sha256: before, final_source_sha256: after,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest, plan_path: PLAN, plan_sha256: Digest::SHA256.file(PLAN).hexdigest,
      outer_resolution_available: @available, fixture_rows: 32, jobs: selected, limits: LIMITS,
      semantic: semantic, semantic_sha256: Digest::SHA256.hexdigest(JSON.generate(semantic)),
      observations: observations, storage: storage, churn: retained,
      notes: ['No elapsed-time samples; all four cursor/eager APIs drain externally.',
        'Every complete outcome equals the resolution-disabled control, including metadata, costs, errors and consumed prefixes.',
        'Each plan signature describes the current binding. The diagnostic does not assume changing a binding must change the winning plan.',
        'The private 32-row fixture is separate from the unchanged 1000-row main benchmark.',
        'New LEFT targets retain bounded resolution only: physical template counts remain zero and warm calls still invoke the base planner.',
        'Cache weak references are counted only after a completed worker thread and full GC; observed VM retention is reported verbatim.'] }
    File.write(options[:report], JSON.generate(report) + "\n")
    puts "Outer join #{options[:jit]}: #{semantic.values.sum { |item| item[:workloads].length }} memory/file API groups passed"
  ensure
    database&.close
  end
end

OuterJoinDiagnostics.main if $PROGRAM_NAME == __FILE__
