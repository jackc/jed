#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit-only comparison of the current Ruby APIs. Each invocation owns one
# engine, and validation uses separate callbacks/database from timed execution.
require 'json'
require 'optparse'
require 'rbconfig'
require 'time'
require_relative 'source'

module RubyBindingComparison
  ENGINES = %w[native-prepared native-query native-eager wrapper].freeze
  REPETITIONS = 5
  module_function

  def environment(engine, counters = nil)
    scalar = if counters
      lambda do |value|
        counters[:row_calls] += 1
        (value * 2_654_435_761).remainder(1_000_003)
      end
    else
      ->(value) { (value * 2_654_435_761).remainder(1_000_003) }
    end
    batch = if counters
      lambda do |values, output|
        counters[:batch_calls] += 1
        counters[:batch_sizes] << values.length
        values.each { |value| output << (value * 2_654_435_761).remainder(1_000_003) }
      end
    else
      lambda do |values, output|
        values.each { |value| output << (value * 2_654_435_761).remainder(1_000_003) }
      end
    end
    if engine == 'wrapper'
      Jed::Environment.build do |builder|
        builder.register_function('bench_mix_row', [:i32], :i64, volatility: :immutable, &scalar)
        builder.register_batch_function('bench_mix_batch', [:i32], :i64, volatility: :immutable, &batch)
      end
    else
      JedNative::Environment.new
        .register_function(name: 'bench_mix_row', args: ['i32'], returns: 'i64', volatility: :immutable, &scalar)
        .register_function(name: 'bench_mix_batch', args: ['i32'], returns: 'i64', volatility: :immutable,
          batch_kernel: ->(columns, output) { batch.call(columns.first, output) })
    end
  end

  def with_session(engine, rows, counters = nil, file: nil)
    if file
      database = engine == 'wrapper' ? Jed.open(file, read_only: true) : JedNative.open(file, read_only: true)
    else
      env = environment(engine, counters)
      database = engine == 'wrapper' ? Jed.memory(environment: env) : JedNative.memory(environment: env)
    end
    session = engine == 'wrapper' ? database : database.session
    session.engine.instance_variable_set(:@native_pipeline_disabled, true) if engine != 'wrapper' && @disable_pipeline
    session.engine.instance_variable_set(:@native_ordering_disabled, true) if engine != 'wrapper' && @disable_ordering
    session.engine.instance_variable_set(:@native_aggregate_disabled, true) if engine != 'wrapper' && @disable_aggregate
    session.engine.instance_variable_set(:@native_relation_disabled, true) if engine != 'wrapper' && @disable_relation
    session.engine.instance_variable_set(:@native_text_compilation_disabled, true) if engine != 'wrapper' && @disable_text
    return yield(session) if file
    session.execute('CREATE TABLE items (id i32 PRIMARY KEY, bucket i32, score i32, label text)')
    values = rows.times.map { |i| "(#{i},#{i % 10},#{(i * 37) % 10_000},'row#{i}')" }.join(',')
    session.execute("INSERT INTO items VALUES #{values}")
    session.execute('CREATE TABLE host_items (id i32 PRIMARY KEY, bucket i32, amount i32)')
    values = rows.times.map { |i| "(#{i},#{i % 10},#{(i * 37) % 10_000})" }.join(',')
    session.execute("INSERT INTO host_items VALUES #{values}")
    session.execute('CREATE INDEX host_items_bucket ON host_items (bucket)')
    yield session
  ensure
    session&.close unless session.equal?(database)
    database&.close
  end

  def workloads(plan)
    jobs = plan.fetch('workloads').map do |workload|
      cheap = %w[scalar pk_hit pk_miss].include?(workload.fetch('name'))
      workload.merge('warmup' => cheap ? 300 : 100, 'iterations' => cheap ? 2000 : 50,
                     'validation_iterations' => 32, 'kind' => 'original')
    end
    jobs << { 'name' => 'host_scalar', 'sql' => 'SELECT bench_mix_row($1::i32)',
              'offset' => -999, 'step' => 17, 'span' => 2000,
              'warmup' => 300, 'iterations' => 2000, 'validation_iterations' => 3, 'kind' => 'host_scalar' }
    %w[buffered streamed].each do |site|
      predicate = site == 'buffered' ? 'bucket' : 'id'
      %w[none row batch].each do |kernel|
        projection = kernel == 'none' ? 'amount' : "bench_mix_#{kernel}(amount)"
        jobs << { 'name' => "host_#{site}_#{kernel}",
                  'sql' => "SELECT id, #{projection} FROM host_items WHERE #{predicate} BETWEEN $1 AND $2 ORDER BY id",
                  'fixed_params' => [0, site == 'buffered' ? 4 : 999],
                  'warmup' => 100, 'iterations' => 50, 'validation_iterations' => 3,
                  'kind' => 'host_rows', 'site' => site, 'kernel' => kernel }
      end
    end
    jobs
  end

  def file_workloads
    [{ 'name' => 'file_pk',
       'sql' => 'SELECT id, customer_id, amount, note FROM orders WHERE id = $1',
       'offset' => 1, 'step' => 7919, 'span' => 1_000_000,
       'warmup' => 50_000, 'iterations' => 5000, 'validation_iterations' => 32, 'kind' => 'file_pk' }]
  end

  def cold_workloads
    [
      { 'name' => 'cold_mixed', 'sql' => "SELECT CASE WHEN $1::i64 BETWEEN -1000 AND 0 THEN coalesce(nullif($1::i64, -7), 3) ELSE abs($1::i64) END AS n, (12.5::decimal / 2::decimal)::text AS d, 'é' || 'x' AS t",
        'offset' => -999, 'step' => 17, 'span' => 2000 },
      { 'name' => 'cold_join', 'sql' => 'SELECT a.label, b.score FROM items a JOIN items b ON b.id=a.id WHERE a.id=$1 ORDER BY b.id LIMIT 1',
        'offset' => 0, 'step' => 17, 'span' => 1000 }
    ].map { |job| job.merge('warmup' => 300, 'iterations' => 2000, 'validation_iterations' => 32, 'kind' => 'cold_query') }
  end

  def parameters(workload)
    count = workload.values_at('warmup', 'iterations', 'validation_iterations').max
    Array.new(count) do |index|
      if workload.key?('fixed_params')
        workload.fetch('fixed_params').dup
      elsif workload.fetch('span').zero?
        []
      else
        [workload.fetch('offset') + (index * workload.fetch('step')) % workload.fetch('span')]
      end
    end
  end

  # The timed closures perform no answer hashing or callback instrumentation.
  # Result iteration remains in the measurement in all four API variants.
  def runner(engine, session, workload, fresh_sql: false)
    sql = workload.fetch('sql')
    if fresh_sql
      raise 'fresh SQL requires an unprepared lane' if engine == 'native-prepared'
      sequence = 0
      return lambda do |params|
        text = "#{sql} /* probe #{sequence += 1} */"
        engine == 'native-eager' ? session.execute(text, *params) : session.query(text, *params)
      end
    end
    if engine == 'native-prepared'
      prepared = session.prepare(sql)
      ->(params) { session.query_prepared(prepared, *params) }
    elsif engine == 'native-eager'
      ->(params) { session.execute(sql, *params) }
    else
      ->(params) { session.query(sql, *params) }
    end
  end

  def collect(engine, run, params)
    result = run.call(params)
    rows = []
    iterable = engine == 'native-eager' ? result.rows : result
    iterable.each { |row| rows << row.to_a }
    { columns: result.columns, column_types: result.column_types, rows: rows, cost: result.cost }
  ensure
    result&.close if result.respond_to?(:close)
  end

  def drain(engine, run, params)
    result = run.call(params)
    count = 0
    iterable = engine == 'native-eager' ? result.rows : result
    iterable.each { count += 1 }
    count
  ensure
    result&.close if result.respond_to?(:close)
  end

  def validate(engine, plan, jobs, file: nil, fresh_sql: false)
    counters = { row_calls: 0, batch_calls: 0, batch_sizes: [] }
    with_session(engine, plan.fetch('rows'), counters, file: file) do |session|
      jobs.map do |job|
        counters[:row_calls] = counters[:batch_calls] = 0
        counters[:batch_sizes].clear
        run = runner(engine, session, job, fresh_sql: fresh_sql)
        params = parameters(job).first(job.fetch('validation_iterations'))
        answers = params.map { |args| collect(engine, run, args) }
        { name: job.fetch('name'), sql: job.fetch('sql'), kind: job.fetch('kind'),
          validation_parameters: params, validation_rows: answers.map { |answer| answer.fetch(:rows).length },
          validation_costs: answers.map { |answer| answer.fetch(:cost) },
          validation_columns: answers.first.fetch(:columns), validation_column_types: answers.first.fetch(:column_types),
          validation_sha256: Digest::SHA256.hexdigest(JSON.generate(answers)),
          answer_sha256: Digest::SHA256.hexdigest(JSON.generate(answers.map { |answer| answer.fetch(:rows) })),
          callback_counts: { row_calls: counters[:row_calls], batch_calls: counters[:batch_calls],
                             batch_sizes: counters[:batch_sizes].dup },
          warmup: job.fetch('warmup'), iterations: job.fetch('iterations'), samples_ns: [], median_ns: nil }
      end
    end
  end

  def time_jobs(engine, plan, jobs, results, file: nil, fresh_sql: false)
    with_session(engine, plan.fetch('rows'), file: file) do |session|
      jobs.each_with_index do |job, index|
        run = runner(engine, session, job, fresh_sql: fresh_sql)
        params = parameters(job)
        job.fetch('warmup').times { |i| drain(engine, run, params[i]) }
        row_counts = []
        samples = REPETITIONS.times.map do
          rows = 0
          started = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)
          job.fetch('iterations').times { |i| rows += drain(engine, run, params[i]) }
          elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond) - started
          row_counts << rows
          elapsed
        end
        results[index][:samples_ns] = samples
        results[index][:rows_per_sample] = row_counts
        results[index][:median_ns] = samples.sort[REPETITIONS / 2].fdiv(job.fetch('iterations'))
      end
    end
  end

  def main
    engine = ARGV.shift
    options = { verify_only: false }
    OptionParser.new do |parser|
      parser.on('--file PATH') { |path| options[:file] = File.expand_path(path) }
      parser.on('--verify-only') { options[:verify_only] = true }
      parser.on('--workloads LIST', Array) { |names| options[:workloads] = names }
      parser.on('--fresh-sql') { options[:fresh_sql] = true }
      parser.on('--disable-pipeline') { options[:disable_pipeline] = true }
      parser.on('--disable-ordering') { options[:disable_ordering] = true }
      parser.on('--disable-aggregate') { options[:disable_aggregate] = true }
      parser.on('--disable-relation') { options[:disable_relation] = true }
      parser.on('--disable-text') { options[:disable_text] = true }
    end.parse!
    abort "Usage: #{File.basename(__FILE__)} #{ENGINES.join('|')} [--verify-only] [--file PATH]" unless ENGINES.include?(engine) && ARGV.empty?
    root = File.expand_path('../..', __dir__)
    source = if engine == 'wrapper'
      File.join(root, 'impl/ruby')
    else
      File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(root, 'impl/ruby-native')))
    end
    source_hash = NativeRubySource.digest(source)
    require File.join(source, engine == 'wrapper' ? 'lib/jed' : 'lib/jed_native')
    @disable_pipeline = !!options[:disable_pipeline]
    abort 'Source has no pipeline compiler to disable' if @disable_pipeline && engine != 'wrapper' && !defined?(JedNative::CompiledPipeline)
    @disable_ordering = !!options[:disable_ordering]
    abort 'Source has no ordering compiler to disable' if @disable_ordering && engine != 'wrapper' && !defined?(JedNative::CompiledOrdering)
    @disable_aggregate = !!options[:disable_aggregate]
    abort 'Source has no aggregate compiler to disable' if @disable_aggregate && engine != 'wrapper' && !defined?(JedNative::CompiledAggregate)
    @disable_relation = !!options[:disable_relation]
    abort 'Source has no relation compiler to disable' if @disable_relation && engine != 'wrapper' && !defined?(JedNative::CompiledRelation)
    @disable_text = !!options[:disable_text]
    abort 'Source has no text compiler to disable' if @disable_text && engine != 'wrapper' && !JedNative::CompiledExpression::TYPES.include?('text')
    library = engine == 'wrapper' ? File.expand_path(Jed::FFI.lib_path) : nil
    library_hash = library && Digest::SHA256.file(library).hexdigest
    plan_path = File.join(__dir__, 'compare/plan.json')
    plan = JSON.parse(File.read(plan_path))
    jobs = options[:file] ? file_workloads : workloads(plan)
    jobs += cold_workloads if options[:fresh_sql] && !options[:file]
    additional_plan_path = nil
    additional_plans = {}
    if options[:workloads] && !options[:file]
      # Additional compiler controls are opt-in within this opt-in
      # harness. Selecting an existing workload leaves its schedule unchanged.
      %w[pipeline ordering aggregate relation text].each do |kind|
        missing = options[:workloads] - jobs.map { |job| job.fetch('name') }
        break if missing.empty?
        path = File.join(__dir__, "compare/#{kind}-plan.json")
        additional_plan = JSON.parse(File.read(path))
        additional_jobs = additional_plan.fetch('workloads').select { |job| missing.include?(job.fetch('name')) }
        unless additional_jobs.empty?
          raise 'additional plan uses a different fixture' unless additional_plan.fetch('rows') == plan.fetch('rows')
          additional_plan_path = path
          additional_plans[path] = Digest::SHA256.file(path).hexdigest
          jobs += additional_jobs.map do |job|
            job.merge('validation_iterations' => additional_plan.fetch('validation_iterations'), 'kind' => "compiled_#{kind}")
          end
        end
      end
    end
    if options[:workloads]
      raise 'unknown or empty workload selection' if options[:workloads].empty? || (options[:workloads] - jobs.map { |job| job['name'] }).any?
      jobs = jobs.select { |job| options[:workloads].include?(job['name']) }
    end
    results = validate(engine, plan, jobs, file: options[:file], fresh_sql: options[:fresh_sql])
    time_jobs(engine, plan, jobs, results, file: options[:file], fresh_sql: options[:fresh_sql]) unless options[:verify_only]
    final_hash = NativeRubySource.digest(source)
    raise 'Ruby source changed during measurement' unless final_hash == source_hash
    raise 'Rust extension changed during measurement' if library && Digest::SHA256.file(library).hexdigest != library_hash

    report = {
      schema: 1, mode: 'ruby_binding_comparison', engine: engine, verify_only: options[:verify_only],
      created_at: Time.now.utc.iso8601, runtime: RUBY_DESCRIPTION, ruby_executable: RbConfig.ruby,
      ruby_executable_sha256: Digest::SHA256.file(RbConfig.ruby).hexdigest,
      yjit: defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?,
      zjit: defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?,
      jit: { yjit: !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?),
             zjit: !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?) },
      source: source, source_sha256: source_hash, final_source_sha256: final_hash,
      native_library: library, native_library_sha256: library_hash,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      plan_path: plan_path, plan_sha256: Digest::SHA256.file(plan_path).hexdigest,
      additional_plan_path: additional_plan_path,
      additional_plan_sha256: additional_plan_path && Digest::SHA256.file(additional_plan_path).hexdigest,
      additional_plans: additional_plans,
      storage: options[:file] ? 'file' : 'memory', file: options[:file],
      fresh_sql: !!options[:fresh_sql],
      pipeline_disabled: @disable_pipeline && engine != 'wrapper',
      ordering_disabled: @disable_ordering && engine != 'wrapper',
      aggregate_disabled: @disable_aggregate && engine != 'wrapper',
      relation_disabled: @disable_relation && engine != 'wrapper',
      text_compilation_disabled: @disable_text && engine != 'wrapper',
      dataset_rows: options[:file] ? 1_000_000 : plan.fetch('rows'), repetitions: REPETITIONS,
      notes: [
        'Opt-in API comparison; the wrapper reparses and materializes each query. Native prepared parsing is outside timing.',
        'Native modes use a long-lived session, matching the wrapper connection lifetime.',
        'Memory SQL/data/parameters match compare/plan.json; warmups/iterations are increased for this diagnostic.',
        'Expression/pipeline/ordering/aggregate/relation/text shapes require an explicit --workloads selection; existing workload schedules remain unchanged.',
        'Validation uses a separate database and instrumented callbacks. Timed callbacks have no counting or hashing.',
        'All query results are fully drained, including the eager API. Validation digests include values, metadata and costs.',
        'File mode uses read-only auto-locking opens, 50000 warmups and 5 batches of 5000 deterministic lookups.',
        'Host buffered site has a bucket index and selects 500 rows; streamed site selects 1000 rows. Actual batching is recorded in validation.'
      ],
      workloads: results
    }
    puts JSON.pretty_generate(report)
  end
end

RubyBindingComparison.main if $PROGRAM_NAME == __FILE__
