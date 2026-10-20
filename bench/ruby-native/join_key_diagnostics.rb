#!/usr/bin/env ruby
# frozen_string_literal: true

# Untimed source-paired key ownership, admission, and semantic checks.
require 'json'
require 'digest'
require 'optparse'
require 'rbconfig'
require 'fileutils'
require 'tmpdir'
require_relative 'source'
require_relative 'join_functions_diagnostics'

module JoinKeyDiagnostics
  ROOT = File.expand_path('../..', __dir__)
  PLAN = File.join(__dir__, 'compare/nway-frontier-plan.json')
  APIS = %i[query_prepared query execute_prepared execute].freeze
  POLICIES = %w[ordinary physical_disabled].freeze
  STATES = %w[cold warm changed null absent warm_again].freeze
  JOBS = %w[scalar pk_hit indexed_join three_table_join_range four_table_join three_table_left_fence five_table_bounded].freeze
  BUDGET_JOBS = %w[three_table_join_range three_table_left_fence five_table_bounded].freeze
  module_function

  def jobs
    source = JSON.parse(File.read(PLAN)).fetch('workloads')
    JOBS.map { |name| source.find { |job| job['name'] == name }.slice('name', 'sql') }
  end

  def bindings(name)
    %w[three_table_join_range five_table_bounded].include?(name) ? [1, 1, 8, nil, 0, 1] : [0, 0, 17, nil, 1000, 0]
  end

  def size(value)
    units, bytes, pending = 0, 0, [value]
    until pending.empty?
      item = pending.pop
      units += 1
      case item
      when Array then pending.concat(item)
      when Hash then item.each { |key, child| pending << key << child }
      when String then bytes += item.bytesize
      end
    end
    [units, bytes]
  end

  def frozen_tree?(value) = JoinFunctionsDiagnostics.snapshot_frozen?(value)

  def occupancy(session)
    entries = (session.engine.instance_variable_get(:@native_join_resolution_plans) || {}).values
    raise 'entry limit exceeded' if entries.length > 64
    values = entries.map do |entry|
      nodes = entry.nodes.keys.map { |node| size(node) }
      roots = entry.roots.keys.map { |key| size(key) }
      if @available
        pool = entry.scope_columns
        descriptors = pool.keys.map { |column| size(column) }
        scope_units = entry.scopes.keys.sum { |scope| 1 + scope.length }
        scope_bytes = 0
        descriptor_units = descriptors.sum(&:first) + 2 * descriptors.length
        descriptor_bytes = descriptors.sum(&:last)
        raise 'descriptor pool does not share owned identity' unless pool.all? { |key, value| key.equal?(value) && frozen_tree?(key) }
        raise 'scope contains an unpooled descriptor' unless entry.scopes.keys.all? do |scope|
          scope.frozen? && scope.all? { |column| pool[column].equal?(column) }
        end
      else
        descriptors = []
        measured = entry.scopes.keys.map { |scope| size(scope) }
        scope_units, scope_bytes = measured.sum(&:first), measured.sum(&:last)
        descriptor_units = descriptor_bytes = 0
      end
      root_units = roots.sum(&:first) + entry.roots.length
      key_units = root_units + nodes.sum(&:first) + scope_units + descriptor_units
      key_bytes = roots.sum(&:last) + nodes.sum(&:last) + scope_bytes + descriptor_bytes
      raise 'owned key accounting differs' unless [key_units, key_bytes] == [entry.key_units, entry.key_bytes]
      raise 'retained key exceeds bound' unless key_units <= 4096 && key_bytes <= 65_536 && entry.roots.length <= 128
      raise 'interned key count exceeds roots' unless entry.nodes.length <= entry.roots.length && entry.scopes.length <= entry.roots.length
      raise 'mutable key retained' unless (entry.roots.keys + entry.nodes.keys + entry.scopes.keys).all? { |key| frozen_tree?(key) }
      metadata = entry.sources.to_a + entry.columns.to_a
      metadata_sizes = metadata.map { |value| size(value) }
      raise 'shared source metadata accounting differs' unless
        [entry.source_units, entry.source_bytes] == [metadata_sizes.sum(&:first), metadata_sizes.sum(&:last)]
      raise 'source metadata exceeds bounds' unless entry.source_units <= 4096 && entry.source_bytes <= 65_536 &&
        entry.sources.length <= 8 && entry.columns.length <= 8 && metadata.all? { |value| frozen_tree?(value[0]) && frozen_tree?(value[1]) }
      { roots: entry.roots.length, nodes: nodes.length, scopes: entry.scopes.length,
        shared_descriptors: descriptors.length, descriptor_references: entry.scopes.keys.sum(&:length),
        distinct_scope_descriptors: entry.scopes.keys.flatten(1).uniq.length,
        key_units: key_units, key_bytes: key_bytes, root_units: root_units,
        node_units: nodes.sum(&:first), node_bytes: nodes.sum(&:last),
        scope_units: scope_units, scope_bytes: scope_bytes,
        descriptor_units: descriptor_units, descriptor_bytes: descriptor_bytes,
        source_units: entry.source_units, source_bytes: entry.source_bytes,
        physical: !entry.physical.nil?, shared_identity_verified: @available,
        immutable_snapshots: true, accounting_verified: true }
    end
    raise 'execution frame escaped' if session.engine.instance_variable_get(:@native_join_resolution_frame)
    { entries: values.length, per_entry: values }
  end

  def outcome(session, statement, sql, params, api, trace: false, **options)
    calls, reasons, plans, frames, stack = Hash.new(0), Hash.new(0), [], [], []
    tracker = TracePoint.new(:call, :return) do |event|
      next unless event.path.start_with?(File.join(@source, 'lib/'))
      owner = event.defined_class.to_s
      if event.event == :call
        calls['original_compile'] += 1 if owner == 'JedNative::Engine' && event.method_id == :compile
        if owner == 'JedNative::PreparedJoins' && event.method_id == :prepared_join_root
          entry = event.binding.local_variable_get(:frame).entry
          stack << { entry: entry, roots: entry.roots.length, units: entry.key_units, bytes: entry.key_bytes,
            nodes: entry.nodes.length, scopes: entry.scopes.length,
            descriptors: @available ? entry.scope_columns.length : 0 }
          calls['root_lookups'] += 1
        end
      elsif owner == 'JedNative::PreparedJoins'
        case event.method_id
        when :prepared_join_key_size, :prepared_join_key_snapshot
          stack.last[:size_attempted] = true if stack.last
          stack.last[:size_declined] = event.return_value.nil? if stack.last
        when :prepared_join_root
          start = stack.pop
          next unless start
          entry = start[:entry]
          saved = event.binding.local_variable_get(:saved)
          reason = if saved && saved[0].equal?(event.return_value)
                     'hit'
                   elsif entry.roots.length > start[:roots]
                     'retained'
                   elsif !event.binding.local_variable_get(:admitted)
                     'unsupported'
                   elsif event.self.cost != event.binding.local_variable_get(:before)
                     'metered'
                   elsif start[:roots] >= 128
                     'root_limit'
                   elsif start[:size_declined]
                     'key_publication_refused'
                   else
                     'other'
                   end
          reasons[reason] += 1
          if start[:size_declined]
            raise 'declined publication changed root count/budget' unless
              [entry.roots.length, entry.key_units, entry.key_bytes] == start.values_at(:roots, :units, :bytes)
            raise 'declined publication left interned fragments' unless
              [entry.nodes.length, entry.scopes.length, @available ? entry.scope_columns.length : 0] ==
                start.values_at(:nodes, :scopes, :descriptors)
          end
        when :prepared_join_frame
          frames << event.return_value.entry.object_id if event.return_value
        when :prepared_join_physical_plan
          saved = event.binding.local_variable_get(:saved)
          calls['physical_hits'] += 1 if saved && saved.plan.equal?(event.return_value)
        when :nway_join_plan, :two_base_join_plan
          plans << JoinFunctionsDiagnostics.plan_image(event.return_value)
        end
      end
    end if trace
    tracker&.enable
    JoinFunctionsDiagnostics.outcome(session, statement, sql, params, api, **options)
  ensure
    tracker&.disable
    raise 'unbalanced root trace' if trace && !stack.empty?
    @last_trace = { calls: calls, reasons: reasons, physical_plans: plans.uniq,
      frame_ids: frames, occupancy: occupancy(session) } if trace
  end

  def pair(database, sql, policy)
    sessions = [database.session, database.session]
    sessions.last.engine.instance_variable_set(:@native_join_resolution_disabled, true)
    sessions.each { |session| session.engine.instance_variable_set(:@native_join_physical_disabled, true) } if policy == 'physical_disabled'
    [sessions, sessions.map { |session| session.prepare(sql) }]
  end

  def compare(sessions, statements, sql, params, api, trace: false, **options)
    left = outcome(sessions[0], statements[0], sql, params, api, trace: trace, **options)
    enabled = @last_trace if trace
    right = outcome(sessions[1], statements[1], sql, params, api, trace: trace, **options)
    disabled = @last_trace if trace
    raise "exact outcome changed: #{api} #{sql} #{params.inspect}" unless left == right
    if trace
      raise 'current plan changed' unless enabled[:physical_plans] == disabled[:physical_plans]
      raise 'disabled cache retained entries' unless disabled[:occupancy][:entries].zero?
      @last_pair = { enabled: enabled, disabled: disabled }
    end
    left
  end

  def fixture(database)
    setup = database.session
    setup.execute('CREATE TABLE items(id i32 PRIMARY KEY,bucket i32,score i32,label text)')
    10.times do |batch|
      values = (batch * 100...(batch + 1) * 100).map { |i| "(#{i},#{i % 10},#{i * 37},'row#{i}')" }.join(',')
      setup.execute("INSERT INTO items VALUES #{values}")
    end
  ensure
    setup&.close
  end

  def workloads(database)
    sessions = nil
    semantic, observations = [], []
    jobs.each do |job|
      APIS.product(POLICIES).each do |api, policy|
        sql = job.fetch('sql')
        sessions, statements = pair(database, sql, policy)
        values, traces = [], []
        STATES.zip(bindings(job.fetch('name'))).each do |state, value|
          actual = compare(sessions, statements, sql, [value], api, trace: true)
          values << { state: state, params: [value], outcome: actual }
          traces << { state: state, params: [value], trace: @last_pair }
        end
        common = { workload: job.fetch('name'), api: api, policy: policy }
        semantic << common.merge(values: values)
        observations << common.merge(values: traces)
        sessions.each(&:close)
      end
    end
    [semantic, observations]
  ensure
    sessions&.each(&:close)
  end

  def budgets(database)
    sessions = nil
    BUDGET_JOBS.product(APIS).map do |name, api|
      sql = jobs.find { |job| job['name'] == name }.fetch('sql')
      sessions, statements = pair(database, sql, 'physical_disabled')
      2.times { compare(sessions, statements, sql, [3], api) }
      ordinary = compare(sessions, statements, sql, [3], api)
      cost = ordinary.fetch(:cost)
      raise 'ordinary budget failed' unless ordinary[:status] == 'rows' && cost.positive?
      limits = [1, cost / 2, cost - 1, cost, cost + 1].select(&:positive?).uniq
      values = %i[max_cost lifetime_max_cost].product(limits).map do |setting, limit|
        { setting: setting, limit: limit, outcome: compare(sessions, statements, sql, [3], api, setting: setting, limit: limit) }
      end
      sessions.each(&:close)
      { workload: name, api: api, params: [3], ordinary: ordinary, limits: values }
    end
  ensure
    sessions&.each(&:close)
  end

  def mutations(database)
    sessions, writer = nil, nil
    sql = jobs.find { |job| job['name'] == 'five_table_bounded' }.fetch('sql')
    sessions, statements = pair(database, sql, 'physical_disabled')
    writer = database.session
    semantic, observations = [], []
    [nil, 'ANALYZE items', 'UPDATE items SET score=9 WHERE id=0',
      'CREATE INDEX join_key_bucket ON items(bucket)', 'DROP INDEX join_key_bucket'].each do |action|
      writer.execute(action) if action
      [1, 8, nil].each do |value|
        actual = compare(sessions, statements, sql, [value], :execute, trace: true)
        observed = @last_pair
        explains = sessions.map { |session| JoinFunctionsDiagnostics.outcome(session, nil, "EXPLAIN #{sql}", [value], :execute) }
        raise 'current EXPLAIN differs' unless explains.uniq.length == 1
        semantic << { after: action, params: [value], outcome: actual, explain: explains.first }
        observations << { after: action, params: [value], trace: observed }
      end
    end
    [semantic, observations]
  ensure
    sessions&.each(&:close)
    writer&.close
  end

  def main
    database = nil
    options = {}
    OptionParser.new do |parser|
      parser.on('--report PATH') { |path| options[:report] = File.expand_path(path) }
      parser.on('--jit NAME', %w[none yjit zjit]) { |jit| options[:jit] = jit }
    end.parse!
    raise 'specify fresh report and runtime' unless ARGV.empty? && options[:report] && options[:jit] && !File.exist?(options[:report])
    @source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(ROOT, 'impl/ruby-native')))
    before = NativeRubySource.digest(@source)
    require File.join(@source, 'lib/jed_native')
    @available = JedNative::PreparedJoins::Entry.members.include?(:scope_columns)
    jit = { yjit: !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?), zjit: !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?) }
    raise 'JIT mode differs' unless jit == { yjit: options[:jit] == 'yjit', zjit: options[:jit] == 'zjit' }
    semantic, observations, storage = {}, {}, {}
    workspace = File.join(ROOT, '.dev/ruby-native-join-key-diagnostics')
    FileUtils.mkdir_p(workspace)
    Dir.mktmpdir('keys-', workspace) do |directory|
      %w[memory disk].each do |mode|
        path = File.join(directory, 'fixture.jed') if mode == 'disk'
        database = mode == 'memory' ? JedNative.memory : JedNative.create(path, locking: :exclusive)
        fixture(database)
        cases, traces = workloads(database)
        costs = budgets(database)
        changed, changed_traces = mutations(database)
        semantic[mode] = { workloads: cases, budgets: costs, mutations: changed }
        observations[mode] = { workloads: traces, mutations: changed_traces }
        storage[mode] = mode == 'memory' ? { kind: 'memory' } : { kind: 'file', locking: 'exclusive', path: path,
          bytes: File.size(path), sha256: Digest::SHA256.file(path).hexdigest, filesystem_device: File.stat(path).dev }
        database.close
      end
    end
    storage['disk'][:removed_after_run] = !File.exist?(storage['disk'][:path])
    raise 'fixture remains' unless storage['disk'][:removed_after_run]
    after = NativeRubySource.digest(@source)
    raise 'source changed' unless before == after
    report = { schema: 1, mode: 'untimed_join_key_diagnostic', runtime: RUBY_DESCRIPTION,
      ruby_executable: RbConfig.ruby, ruby_executable_sha256: Digest::SHA256.file(RbConfig.ruby).hexdigest,
      source: @source, source_sha256: before, final_source_sha256: after, jit: jit,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest, plan_path: PLAN, plan_sha256: Digest::SHA256.file(PLAN).hexdigest,
      shared_scope_columns: @available, fixture_rows: 1000, jobs: jobs,
      semantic: semantic, semantic_sha256: Digest::SHA256.hexdigest(JSON.generate(semantic)),
      observations: observations, storage: storage,
      notes: ['No timings or disabled GC; complete values, metadata, error prefixes and work are compared to original resolution.',
        'Each retained descriptor is immutable and shared by exact identity; independent recursive accounting includes pool references and every scope reference within unchanged caps.',
        'A refused key publication may reflect limits, unsupported metadata or depth; only the separate baseline occupancy probe independently classifies unit and byte causes.',
        'Ordinary reuse and physical-disabled groups distinguish physical winner reuse from actual resolved-root hits and remaining compilation.',
        'Compact current plan signatures include choice and estimate fields; exact callback order and host mutation guards are separately covered by focused tests.'] }
    File.write(options.fetch(:report), JSON.generate(report) + "\n")
    puts "JOIN keys #{options[:jit]}: #{semantic.values.sum { |value| value[:workloads].length }} exact memory/file API/policy groups passed"
  ensure
    database&.close
  end
end

JoinKeyDiagnostics.main if $PROGRAM_NAME == __FILE__
