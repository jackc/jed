#!/usr/bin/env ruby
# frozen_string_literal: true

# Untimed SQL equivalence, helper identity/order and allocation observations.
# Source comparison uses separate processes; all existing compilation stays on.
require 'json'
require 'digest'
require 'optparse'
require 'rbconfig'
require 'fileutils'
require 'tmpdir'
require_relative 'source'

module ExpressionWalkDiagnostics
  ROOT = File.expand_path('../..', __dir__)
  PLAN = File.join(__dir__, 'compare/multiway-plan.json')
  APIS = %i[query_prepared query execute_prepared execute].freeze
  METHODS = %i[expression_nodes same_key_expression? conjunctions].freeze
  ORIGINALS = { expression_nodes: 'JedNative::QueryPlans',
    same_key_expression?: 'JedNative::IndexScans', conjunctions: 'JedNative::IndexScans' }.freeze
  MAIN_JOBS = %w[indexed_join three_table_join three_table_join_range].freeze
  EXTRA_JOBS = [
    { name: 'nested_key', sql: 'SELECT id, label FROM expr_items WHERE (a + b) * 2 = $1 ORDER BY id', index: 'ew_nested', matched: true },
    { name: 'reversed_key', sql: 'SELECT id, label FROM expr_items WHERE $1 = (a + b) * 2 ORDER BY id', index: 'ew_nested', matched: true },
    { name: 'different_key', sql: 'SELECT id, label FROM expr_items WHERE (b + a) * 2 = $1 ORDER BY id', index: 'ew_nested', matched: false },
    { name: 'partial_match', sql: 'SELECT id, label FROM expr_items WHERE a + b = $1 AND enabled = true ORDER BY id', index: 'ew_partial', matched: true },
    { name: 'partial_mismatch', sql: 'SELECT id, label FROM expr_items WHERE a + b = $1 AND enabled = false ORDER BY id', index: 'ew_partial', matched: false },
    { name: 'compound_text', sql: "SELECT id, label FROM expr_items WHERE bucket = 2 AND lower(label) = $1 ORDER BY id", index: 'ew_compound', matched: true }
  ].freeze
  BUDGET_JOBS = %w[three_table_join nested_key partial_match].freeze
  STATES = %w[cold warm changed absent null warm_again].freeze
  ALLOCATION_ITERATIONS = 128
  ALLOCATION_SAMPLES = 5
  module_function

  def jobs
    main = JSON.parse(File.read(PLAN)).fetch('workloads').select { |job| MAIN_JOBS.include?(job.fetch('name')) }
    main.map { |job| job.slice('name', 'sql').transform_keys(&:to_sym) } + EXTRA_JOBS
  end

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

  def bindings(name)
    return ['row2', 'row2', 'row6', 'absent', nil, 'row2'] if name == 'compound_text'
    return [1, 1, 17, 0, nil, 1] if name == 'three_table_join_range'
    return [10, 10, 12, 999, nil, 10] if EXTRA_JOBS.any? { |job| job[:name] == name }
    [0, 0, 17, 1001, nil, 0]
  end

  def pair(database, sql)
    sessions = [database.session, database.session]
    sessions.last.engine.instance_variable_set(:@native_expression_walks_disabled, true)
    [sessions, sessions.map { |session| session.prepare(sql) }]
  end

  def outcome(session, statement, sql, params, api, trace: false, setting: nil, limit: nil)
    rows, phase, calls = [], 'admission', Hash.new(0)
    lifetime = session.lifetime_cost
    session.public_send("#{setting}=", setting == :lifetime_max_cost ? lifetime + limit : limit) if setting
    tracker = TracePoint.new(:call) do |event|
      next unless event.path.start_with?(File.join(@source, 'lib/'))
      klass = event.defined_class.to_s
      next unless klass == 'JedNative::ExpressionWalks' || ORIGINALS[event.method_id] == klass
      calls["#{klass}##{event.method_id}"] += 1
    end if trace
    tracker&.enable
    result = api.to_s.end_with?('_prepared') ? session.public_send(api, statement, *params) : session.public_send(api, sql, *params)
    admission_columns, admission_types = result.columns.dup, result.column_types.dup
    phase = 'pull'
    result.each { |row| rows << image(row.to_a) }
    [{ status: 'rows', rows: rows, columns: result.columns, types: result.column_types,
      admission_columns: admission_columns, admission_types: admission_types,
      cost: result.cost, lifetime_cost: session.lifetime_cost - lifetime }, calls]
  rescue JedNative::Error => error
    [{ status: 'error', phase: phase, sqlstate: error.sqlstate, message: error.message,
      rows: rows, admission_columns: admission_columns, admission_types: admission_types,
      cost: result&.cost || session.engine.cost, lifetime_cost: session.lifetime_cost - lifetime }, calls]
  ensure
    tracker&.disable
    result&.close
    session.max_cost = session.lifetime_max_cost = 0
  end

  def compare(sessions, statements, sql, params, api, **options)
    actual, enabled = outcome(sessions.first, statements.first, sql, params, api, **options)
    expected, disabled = outcome(sessions.last, statements.last, sql, params, api, **options)
    raise "helper changed #{api} #{sql} #{params.inspect}" unless actual == expected
    if options[:trace] && @available
      ORIGINALS.each do |method, klass|
        raise "ordinary SQL entered original #{method}" unless enabled.fetch("#{klass}##{method}", 0).zero?
      end
    end
    { outcome: actual, calls: options[:trace] ? { enabled: enabled, disabled: disabled } : nil }.compact
  end

  def fixture(database)
    session = database.session
    session.execute('CREATE TABLE items(id i32 PRIMARY KEY, bucket i32, score i32, label text)')
    values = 1000.times.map { |i| "(#{i},#{i % 10},#{i * 37 % 10_000},'row#{i}')" }.join(',')
    session.execute("INSERT INTO items VALUES #{values}")
    session.execute('CREATE TABLE expr_items(id i32 PRIMARY KEY, a i32, b i32, bucket i32, label text, enabled boolean)')
    values = 128.times.map do |i|
      a = i % 17 == 0 ? 'NULL' : (i % 13).to_s
      "(#{i},#{a},#{i % 7},#{i % 4},'RoW#{i}',#{i.even? ? 'true' : 'false'})"
    end.join(',')
    session.execute("INSERT INTO expr_items VALUES #{values}")
    session.execute('CREATE INDEX ew_nested ON expr_items (((a + b) * 2))')
    session.execute('CREATE INDEX ew_partial ON expr_items ((a + b)) WHERE enabled = true')
    session.execute('CREATE INDEX ew_compound ON expr_items (bucket, lower(label))')
  ensure
    session&.close
  end

  def workload_cases(database, selected)
    sessions = nil
    selected.flat_map do |job|
      APIS.map do |api|
        sessions, statements = pair(database, job.fetch(:sql))
        values = bindings(job.fetch(:name)).each_with_index.map do |value, index|
          { state: STATES.fetch(index), params: [value] }.merge(
            compare(sessions, statements, job[:sql], [value], api, trace: true))
        end
        sessions.each(&:close)
        { workload: job[:name], api: api, values: values }
      end
    end
  ensure
    sessions&.each(&:close)
  end

  def explain_cases(database, selected)
    sessions = nil
    selected.map do |job|
      sql = "EXPLAIN #{job.fetch(:sql)}"
      sessions, statements = pair(database, sql)
      params = [bindings(job.fetch(:name)).first]
      answer = compare(sessions, statements, sql, params, :execute_prepared).fetch(:outcome)
      raise 'EXPLAIN failed' unless answer[:status] == 'rows'
      if job[:index]
        matched = JSON.generate(answer[:rows]).include?(job[:index])
        raise "unexpected index choice #{job[:name]}" unless matched == job[:matched]
      end
      sessions.each(&:close)
      { workload: job[:name], params: params, outcome: answer }
    end
  ensure
    sessions&.each(&:close)
  end

  def budget_cases(database, selected)
    sessions = nil
    BUDGET_JOBS.flat_map do |name|
      job = selected.find { |item| item[:name] == name }
      sql, params = job.fetch(:sql), [bindings(name).first]
      APIS.map do |api|
        sessions, statements = pair(database, sql)
        2.times { compare(sessions, statements, sql, params, api) }
        ordinary = compare(sessions, statements, sql, params, api).fetch(:outcome)
        raise 'budget ordinary query failed' unless ordinary[:status] == 'rows'
        cost = ordinary.fetch(:cost)
        values = [1, [cost / 2, 1].max, [cost - 1, 1].max, cost, cost + 1].uniq.sort.flat_map do |limit|
          %i[max_cost lifetime_max_cost].map do |setting|
            { setting: setting, limit: limit }.merge(compare(sessions, statements, sql, params, api, setting: setting, limit: limit))
          end
        end
        sessions.each(&:close)
        { workload: name, api: api, params: params, ordinary: ordinary, values: values }
      end
    end
  ensure
    sessions&.each(&:close)
  end

  def node(kind, value, args = [], type = 'i32', extra = nil)
    JedNative::Expression.new(kind: kind, type: type, value: value, args: args, extra: extra)
  end

  def allocation_probe(engine, method, arguments)
    300.times { engine.send(method, *arguments) }
    ALLOCATION_SAMPLES.times.map do
      GC.start
      before, count = GC.stat(:total_allocated_objects), 0
      while count < ALLOCATION_ITERATIONS
        engine.send(method, *arguments)
        count += 1
      end
      GC.stat(:total_allocated_objects) - before
    end
  end

  def helper_cases
    engines = [JedNative::Engine.new, JedNative::Engine.new]
    engines.last.instance_variable_set(:@native_expression_walks_disabled, true)
    leaf = node(:column, 0)
    tree = 8.times.reduce(leaf) { |left, i| node(:binary, '+', [left, node(:literal, i)]) }
    forest = [tree, [leaf, node(:call, 'abs', [tree])], nil, :ignored]
    conjunction = 16.times.reduce(node(:binary, '=', [leaf, node(:literal, 1)], 'boolean')) do |left, i|
      node(:binary, 'and', [left, node(:binary, '=', [leaf, node(:literal, i)], 'boolean')], 'boolean')
    end
    copy = Marshal.load(Marshal.dump(tree))
    different = Marshal.load(Marshal.dump(tree))
    different.args.last.value = 99
    cast = node(:cast, nil, [leaf], 'decimal', { type_mod: { precision: 10, scale: 2 } })
    cast_other = node(:cast, nil, [leaf], 'decimal', { type_mod: { precision: 10, scale: 3 } })
    body_left = node(:column, 0, [], 'i32', { native_body_type: 'i32' })
    body_right = node(:column, 0, [], 'i64')
    cases = [
      ['nodes_tree', :expression_nodes, [tree]], ['nodes_nested_array', :expression_nodes, [forest]],
      ['conjunctions', :conjunctions, [conjunction]],
      ['key_equal', :same_key_expression?, [tree, copy]],
      ['key_unequal', :same_key_expression?, [tree, different]],
      ['key_typemod', :same_key_expression?, [cast, cast_other]],
      ['key_native_body_type', :same_key_expression?, [body_left, body_right]],
      ['key_nil_args', :same_key_expression?, [node(:literal, 3, nil), node(:literal, 3)]]
    ]
    cases.map do |name, method, arguments|
      actual, expected = engines.map { |engine| engine.send(method, *arguments) }
      if actual.is_a?(Array)
        raise 'helper order/identity changed' unless actual.length == expected.length && actual.zip(expected).all? { |a, b| a.equal?(b) }
        second = engines.first.send(method, *arguments)
        raise 'helper reused caller-owned output Array' if actual.equal?(second)
        answer = actual.map { |entry| [entry.kind, entry.type, image(entry.value)] }
      else
        raise 'key comparison changed' unless actual == expected
        answer = actual
      end
      allocations = engines.map { |engine| allocation_probe(engine, method, arguments) }
      { name: name, method: method, answer: answer, exact_order_identity: true,
        allocations: { enabled: allocations.first, disabled: allocations.last } }
    end
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
    @available = !!defined?(JedNative::ExpressionWalks)
    jit = { yjit: !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?), zjit: !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?) }
    raise 'requested JIT unavailable' unless jit == { yjit: options[:jit] == 'yjit', zjit: options[:jit] == 'zjit' }
    selected, semantic, observations, storage = jobs, {}, {}, {}
    workspace = File.join(ROOT, '.dev/ruby-native-expression-walk-diagnostics')
    FileUtils.mkdir_p(workspace)
    Dir.mktmpdir('expression-', workspace) do |directory|
      %w[memory disk].each do |mode|
        path = File.join(directory, 'fixture.jed') if mode == 'disk'
        database = mode == 'memory' ? JedNative.memory : JedNative.create(path, locking: :exclusive)
        fixture(database)
        cases = workload_cases(database, selected)
        semantic[mode] = { workloads: cases.map do |item|
          item.merge(values: item.fetch(:values).map { |value| value.reject { |key, _| key == :calls } })
        end, explains: explain_cases(database, selected), budgets: budget_cases(database, selected) }
        observations[mode] = cases.map do |item|
          item.slice(:workload, :api).merge(values: item.fetch(:values).map { |value| value.slice(:state, :params, :calls) })
        end
        storage[mode] = mode == 'memory' ? { kind: 'memory' } : {
          kind: 'file', locking: 'exclusive', path: path, bytes: File.size(path),
          sha256: Digest::SHA256.file(path).hexdigest, filesystem_device: File.stat(path).dev }
        database.close
      end
    end
    storage['disk'][:removed_after_run] = !File.exist?(storage.fetch('disk').fetch(:path))
    raise 'private file was not removed' unless storage['disk'][:removed_after_run]
    helpers = helper_cases
    after = NativeRubySource.digest(@source)
    raise 'source changed during diagnostics' unless before == after
    report = { schema: 1, mode: 'untimed_expression_walk_diagnostic', runtime: RUBY_DESCRIPTION,
      ruby_executable: RbConfig.ruby, ruby_executable_sha256: Digest::SHA256.file(RbConfig.ruby).hexdigest,
      jit: jit, source: @source, source_sha256: before, final_source_sha256: after,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest, plan_path: PLAN, plan_sha256: Digest::SHA256.file(PLAN).hexdigest,
      optimized_module: @available, jobs: selected, semantic: semantic,
      semantic_sha256: Digest::SHA256.hexdigest(JSON.generate(semantic)), observations: observations, storage: storage,
      helpers: helpers, allocation_iterations: ALLOCATION_ITERATIONS, allocation_samples: ALLOCATION_SAMPLES,
      notes: ['No elapsed-time samples; GC remains enabled during bounded helper allocation observations.',
        'Each full SQL outcome is retained only after exact enabled/disabled comparison, including metadata, costs, error phase and consumed prefix.',
        'Cost ceilings are representative observations; exact-ceiling success is not assumed uniform across APIs.',
        'EXPLAIN preserves selected plans and verifies matching/nonmatching expression-index cases. Main timing SQL and fixture are unchanged.',
        'Helper cases preserve duplicate node identities, preorder and fresh result Arrays; focused tests cover mutation and unusual subclass behavior.',
        'Allocation counts include common reflective dispatch overhead and are observations rather than whole-query speedup claims.'] }
    File.write(options[:report], JSON.generate(report) + "\n")
    puts "Expression-walk #{options[:jit]}: #{semantic.values.sum { |item| item[:workloads].length }} memory/file API groups passed"
  ensure
    database&.close
  end
end

ExpressionWalkDiagnostics.main if $PROGRAM_NAME == __FILE__
