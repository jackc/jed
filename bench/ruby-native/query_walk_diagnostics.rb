#!/usr/bin/env ruby
# frozen_string_literal: true

# Opt-in, untimed equivalence and call-count evidence. Each case saves one full
# outcome after comparing enabled and disabled walkers/source metadata reuse.
require 'json'
require 'digest'
require 'optparse'
require 'rbconfig'
require 'fileutils'
require 'tmpdir'
require_relative 'source'

module QueryWalkDiagnostics
  ROOT = File.expand_path('../..', __dir__)
  PLAN = File.join(__dir__, 'compare/multiway-plan.json')
  CONTROL_PLAN = File.join(__dir__, 'compare/query-walk-controls.json')
  APIS = %i[query_prepared query execute_prepared execute].freeze
  ORIGINALS = {
    local_aggregate?: 'JedNative::AggregateExecution',
    contains_aggregate?: 'JedNative::Engine',
    window_nodes: 'JedNative::WindowExecution',
    validate_opaque_query_tree: 'JedNative::OpaqueDefinitions'
  }.freeze
  TRACE_METHODS = {
    'JedNative::QueryWalks' => ORIGINALS.keys,
    'JedNative::AggregateExecution' => %i[local_aggregate?],
    'JedNative::Engine' => %i[contains_aggregate?],
    'JedNative::WindowExecution' => %i[window_nodes],
    'JedNative::OpaqueDefinitions' => %i[validate_opaque_query_tree],
    'JedNative::ParameterBindings' => %i[query_columns],
    'JedNative::Session' => %i[statement_writes?],
    'JedNative::JoinPlanner' => %i[two_base_join_plan],
    'JedNative::NWayJoins' => %i[nway_join_plan],
    'JedNative::PreparedJoins' => %i[prepared_join_physical_plan]
  }.freeze
  SOURCE_TARGETS = %w[indexed_join three_table_join three_table_join_miss
    three_table_join_range three_table_computed_key four_table_join].freeze
  FRESH_JOBS = %w[fresh_sql fresh_join].freeze
  module_function

  def image(value)
    case value
    when Array then value.map { |item| image(item) }
    when JedNative::CollatedText
      { ruby_class: value.class.name, text: value.to_s,
        collation: value.collation_label&.to_h&.transform_values do |item|
          item.respond_to?(:sort_key) ? item.class.name : item
        end }
    when String then { ruby_class: value.class.name, text: value }
    else value
    end
  end

  def syntax_units(value)
    case value
    when Hash then 1 + value.sum { |key, child| syntax_units(key) + syntax_units(child) }
    when Array then 1 + value.sum { |child| syntax_units(child) }
    else 1
    end
  end

  def string_bytes(value)
    case value
    when Hash then value.sum { |key, child| string_bytes(key) + string_bytes(child) }
    when Array then value.sum { |child| string_bytes(child) }
    when String then value.bytesize
    else 0
    end
  end

  def owned_frozen?(value)
    case value
    when Hash then value.frozen? && value.all? { |key, child| owned_frozen?(key) && owned_frozen?(child) }
    when Array then value.frozen? && value.all? { |child| owned_frozen?(child) }
    when String then value.frozen?
    when Symbol, Integer, NilClass, TrueClass, FalseClass then true
    else false
    end
  end

  def cache_state(session)
    entries = (session.engine.instance_variable_get(:@native_join_resolution_plans) || {}).values
    raise 'join entry bound exceeded' if entries.length > JedNative::PreparedJoins::CACHE_ENTRIES
    state = entries.map do |entry|
      raise 'source metadata entry unavailable' unless entry.respond_to?(:sources)
      units = entry.sources.sum { |key, columns| syntax_units([key, columns]) }
      bytes = entry.sources.sum { |key, columns| string_bytes([key, columns]) }
      frozen = entry.sources.all? { |key, columns| owned_frozen?(key) && owned_frozen?(columns) }
      raise 'source metadata accounting differs' unless [units, bytes] == [entry.source_units, entry.source_bytes]
      raise 'source metadata bound exceeded' unless entry.sources.length <= JedNative::PreparedJoins::MAX_INPUTS &&
        units <= JedNative::PreparedJoins::KEY_UNITS && bytes <= JedNative::PreparedJoins::KEY_BYTES
      raise 'source metadata owns mutable containers' unless frozen
      { sources: entry.sources.length, units: units, bytes: bytes, owned_frozen: frozen }
    end
    { entries: entries.length, source_entries: state.sum { |item| item[:sources] }, per_entry: state }
  end

  def outcome(session, statement, sql, params, api, trace: false, setting: nil, limit: nil)
    rows, phase, calls = [], 'admission', Hash.new(0)
    lifetime = session.lifetime_cost
    if setting
      session.public_send("#{setting}=", setting == :lifetime_max_cost ? lifetime + limit : limit)
    end
    tracker = TracePoint.new(:call) do |event|
      next unless event.path.start_with?(File.join(@source, 'lib/'))
      metadata = event.method_id == :query_source_columns &&
        %w[JedNative::QueryColumns JedNative::PreparedJoins].include?(event.defined_class.to_s)
      next unless metadata || TRACE_METHODS.fetch(event.defined_class.to_s, []).include?(event.method_id)
      calls["#{event.defined_class}##{event.method_id}"] += 1
    end if trace
    tracker&.enable
    result = api.to_s.end_with?('_prepared') ? session.public_send(api, statement, *params) :
      session.public_send(api, sql, *params)
    admission_columns, admission_types = result.columns.dup, result.column_types.dup
    phase = 'pull'
    result.each { |row| rows << image(row.to_a) }
    answer = { status: 'rows', rows: rows, columns: result.columns, types: result.column_types,
      admission_columns: admission_columns, admission_types: admission_types,
      cost: result.cost, lifetime_cost: session.lifetime_cost - lifetime }
    [answer, calls]
  rescue JedNative::Error => error
    answer = { status: 'error', phase: phase, sqlstate: error.sqlstate, message: error.message,
      rows: rows, admission_columns: admission_columns, admission_types: admission_types,
      cost: result&.cost || session.engine.cost, lifetime_cost: session.lifetime_cost - lifetime }
    [answer, calls]
  ensure
    tracker&.disable
    result&.close
    session.max_cost = session.lifetime_max_cost = 0
  end

  def pair(database, sql)
    sessions = [database.session, database.session]
    sessions.last.engine.instance_variable_set(:@native_query_walks_disabled, true)
    [sessions, sessions.map { |session| session.prepare(sql) }]
  end

  def compare(sessions, statements, sql, params, api, **options)
    actual, enabled = outcome(sessions.first, statements.first, sql, params, api, **options)
    expected, disabled = outcome(sessions.last, statements.last, sql, params, api, **options)
    raise "walker changed #{api} #{sql} #{params.inspect}" unless actual == expected
    if options[:trace]
      ORIGINALS.each do |method, klass|
        original = "#{klass}##{method}"
        replacement = "JedNative::QueryWalks##{method}"
        raise "enabled execution entered original #{method}" unless enabled.fetch(original, 0).zero?
        raise "#{method} visited more nodes" unless enabled.fetch(replacement, 0) <= disabled.fetch(original, 0)
      end
      %w[JedNative::JoinPlanner#two_base_join_plan JedNative::NWayJoins#nway_join_plan].each do |method|
        raise 'walker changed physical-plan admission' unless enabled.fetch(method, 0) == disabled.fetch(method, 0)
      end
      original = 'JedNative::QueryColumns#query_source_columns'
      raise 'source metadata increased original calls' unless enabled.fetch(original, 0) <= disabled.fetch(original, 0)
    end
    states = { enabled: cache_state(sessions.first), disabled: cache_state(sessions.last) } if options[:trace]
    raise 'disabled source metadata control retained entries' if states && states[:disabled][:source_entries].positive?
    { outcome: actual, calls: options[:trace] ? { enabled: enabled, disabled: disabled } : nil,
      cache: states }.compact
  end

  def fixture(database)
    session = database.session
    session.execute('CREATE TABLE items(id i32 PRIMARY KEY, bucket i32, score i32, label text)')
    values = 1000.times.map { |index| "(#{index},#{index % 10},#{index * 37 % 10_000},'row#{index}')" }.join(',')
    session.execute("INSERT INTO items VALUES #{values}")
  ensure
    session&.close
  end

  def bindings(name)
    return [1, 1, 17, nil, 1] if %w[three_table_join_range positive_window ordinary_aggregate].include?(name)
    return [1000, 1000, 1017, nil, 1000] if name == 'three_table_join_miss'
    [0, 0, 17, nil, 0]
  end

  def states(name)
    FRESH_JOBS.include?(name) ? %w[cold fresh_again changed null fresh_final] :
      %w[cold warm changed null warm_again]
  end

  def workload_cases(database, jobs)
    sessions = nil
    jobs.flat_map do |job|
      name, sql = job.values_at('name', 'sql')
      apis = FRESH_JOBS.include?(name) ? %i[query execute] : APIS
      apis.map do |api|
        sessions, statements = pair(database, sql)
        units = syntax_units(statements.first.ast)
        raise 'oversized fixture no longer exceeds 1024 syntax units' if name == 'oversized_syntax' && units <= 1024
        values = bindings(name).each_with_index.map do |value, index|
          selected = FRESH_JOBS.include?(name) ? "#{sql} /* query-walk-direct-#{index} */" : sql
          compared = compare(sessions, statements, selected, [value], api, trace: true)
          { state: states(name).fetch(index), params: [value] }.merge(compared)
        end
        if SOURCE_TARGETS.include?(name)
          raise "#{name} did not retain source metadata" unless values.any? { |value| value[:cache][:enabled][:source_entries].positive? }
          original = 'JedNative::QueryColumns#query_source_columns'
          reused = values.any? do |value|
            %w[warm warm_again].include?(value[:state]) &&
              value[:calls][:enabled].fetch(original, 0) < value[:calls][:disabled].fetch(original, 0)
          end
          raise "#{name} #{api} did not reuse warm source metadata" unless reused
        end
        sessions.each(&:close)
        { workload: name, api: api, syntax_units: units, values: values }
      end
    end
  ensure
    sessions&.each(&:close)
  end

  def budget_cases(database, jobs)
    sessions = nil
    %w[three_table_join three_table_join_range positive_window ordinary_aggregate].flat_map do |name|
      sql = jobs.find { |job| job.fetch('name') == name }.fetch('sql')
      APIS.map do |api|
        sessions, statements = pair(database, sql)
        2.times { compare(sessions, statements, sql, [3], api) }
        ordinary = compare(sessions, statements, sql, [3], api).fetch(:outcome)
        raise 'budget fixture did not return rows' unless ordinary[:status] == 'rows'
        cost = ordinary.fetch(:cost)
        ceilings = [1, [cost / 2, 1].max, [cost - 1, 1].max, cost, cost + 1].uniq.sort
        values = ceilings.flat_map do |limit|
          %i[max_cost lifetime_max_cost].map do |setting|
            { setting: setting, limit: limit }.merge(
              compare(sessions, statements, sql, [3], api, setting: setting, limit: limit))
          end
        end
        sessions.each(&:close)
        { workload: name, api: api, params: [3], ordinary: ordinary, values: values }
      end
    end
  ensure
    sessions&.each(&:close)
  end

  def summary(cases)
    aggregate = { enabled: Hash.new(0), disabled: Hash.new(0) }
    cases.each do |item|
      item.fetch(:values).each do |value|
        value.fetch(:calls).each do |side, counts|
          counts.each { |name, count| aggregate.fetch(side)[name] += count }
        end
      end
    end
    raise 'replacement walkers were not exercised' unless aggregate[:enabled].any? do |name, count|
      name.start_with?('JedNative::QueryWalks#') && count.positive?
    end
    ORIGINALS.each do |method, klass|
      original, replacement = "#{klass}##{method}", "JedNative::QueryWalks##{method}"
      raise "original control #{method} not exercised" unless aggregate[:disabled].fetch(original, 0).positive?
      raise "replacement #{method} not exercised" unless aggregate[:enabled].fetch(replacement, 0).positive?
    end
    aggregate
  end

  def main
    database = nil
    options = {}
    OptionParser.new do |parser|
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
      parser.on('--jit NAME', %w[none yjit zjit]) { |value| options[:jit] = value }
    end.parse!
    abort 'Specify a report path and JIT' unless ARGV.empty? && options[:report] && options[:jit]
    raise 'report exists; preserve prior evidence' if File.exist?(options[:report])
    @source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(ROOT, 'impl/ruby-native')))
    before = NativeRubySource.digest(@source)
    require File.join(@source, 'lib/jed_native')
    raise 'query walkers are unavailable' unless defined?(JedNative::QueryWalks)
    jit = { yjit: !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?),
      zjit: !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?) }
    raise 'requested JIT unavailable' unless jit == { yjit: options[:jit] == 'yjit', zjit: options[:jit] == 'zjit' }
    jobs = [PLAN, CONTROL_PLAN].flat_map { |path| JSON.parse(File.read(path)).fetch('workloads') }
    semantic, observations, storage = {}, {}, {}
    workspace = File.join(ROOT, '.dev/ruby-native-query-walk-diagnostics')
    FileUtils.mkdir_p(workspace)
    Dir.mktmpdir('query-', workspace) do |directory|
      %w[memory disk].each do |mode|
        path = File.join(directory, 'fixture.jed') if mode == 'disk'
        database = mode == 'memory' ? JedNative.memory : JedNative.create(path, locking: :exclusive)
        fixture(database)
        cases = workload_cases(database, jobs)
        budgets = budget_cases(database, jobs)
        counts = summary(cases)
        semantic[mode] = { workloads: cases.map do |item|
          item.merge(values: item.fetch(:values).map { |value| value.reject { |key, _| %i[calls cache].include?(key) } })
        end, budgets: budgets }
        observations[mode] = { calls: cases.map do |item|
          item.slice(:workload, :api).merge(values: item.fetch(:values).map { |value| value.slice(:state, :params, :calls, :cache) })
        end, totals: counts }
        storage[mode] = mode == 'memory' ? { kind: 'memory' } : {
          kind: 'file', locking: 'exclusive', path: path, bytes: File.size(path),
          sha256: Digest::SHA256.file(path).hexdigest, filesystem_device: File.stat(path).dev }
        database.close
      end
    end
    storage['disk'][:removed_after_run] = !File.exist?(storage.fetch('disk').fetch(:path))
    raise 'private file was not removed' unless storage['disk'][:removed_after_run]
    after = NativeRubySource.digest(@source)
    raise 'source changed during diagnostics' unless before == after
    report = { schema: 1, mode: 'untimed_query_walk_diagnostic',
      runtime: RUBY_DESCRIPTION, ruby_executable: RbConfig.ruby,
      ruby_executable_sha256: Digest::SHA256.file(RbConfig.ruby).hexdigest, jit: jit,
      source: @source, source_sha256: before, final_source_sha256: after,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      plans: [PLAN, CONTROL_PLAN].to_h { |path| [path, Digest::SHA256.file(path).hexdigest] },
      limits: { join_entries: JedNative::PreparedJoins::CACHE_ENTRIES,
        sources_per_entry: JedNative::PreparedJoins::MAX_INPUTS,
        source_units: JedNative::PreparedJoins::KEY_UNITS, source_bytes: JedNative::PreparedJoins::KEY_BYTES },
      source_targets: SOURCE_TARGETS,
      jobs: jobs, semantic: semantic, semantic_sha256: Digest::SHA256.hexdigest(JSON.generate(semantic)),
      observations: observations, storage: storage,
      notes: ['No timing samples. The internal control disables fresh walkers and source metadata reuse together; all other optimizations stay enabled.',
        'Every result is externally drained. One full outcome is saved only after exact enabled/control comparison of values, string classes/collation labels, names/types, admission metadata, costs, lifetime costs and error phase/prefix.',
        'The cold/warm labels describe repeated execution. Walkers retain no classification state; source metadata uses the existing bounded join-entry lifetime.',
        'Source metadata counters are independently recounted and all owned key/column containers must be frozen. Base query_source_columns may still run for admission outside a reusable join frame.',
        'Call counts distinguish original helpers from replacement helpers and show fewer visits, not elimination of syntax walks or sampled CPU percentages.',
        'Fresh SQL has a distinct SQL suffix per execution and uses only unprepared query/execute APIs.',
        'Budget cases check representative explicitly listed ceilings, not every possible prefix. Focused tests cover public AST mutation, view/CTE behavior and walker ownership.'] }
    File.write(options[:report], JSON.generate(report) + "\n")
    puts "Query-walk #{options[:jit]}: #{semantic.values.sum { |mode| mode[:workloads].length }} memory/file API cases passed"
  ensure
    database&.close
  end
end

QueryWalkDiagnostics.main if $PROGRAM_NAME == __FILE__
