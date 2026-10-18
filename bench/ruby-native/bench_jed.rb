#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit-only pure Ruby core adapter for the existing shared benchmark corpus.
# Harness dependencies stay in development tooling; the core loads none of them.
$LOAD_PATH.unshift(File.expand_path('../ruby/lib', __dir__))
$LOAD_PATH.unshift(File.expand_path('../../impl/ruby-native/lib', __dir__))

require 'fileutils'
require 'tmpdir'
require 'timeout'
require 'bench'
require 'jed_native'
require_relative 'source'

module NativeRubyBench
  Parameter = Struct.new(*Bench::Param.members, :elem_min, :elem_max)

  def self.environment
    JedNative::Environment.new
      .register_function(name: 'bench_mix_row', args: ['i32'], returns: 'i64', volatility: :immutable) { |value| value * 2_654_435_761 % 1_000_003 }
      .register_function(name: 'bench_mix_batch', args: ['i32'], returns: 'i64', volatility: :immutable,
        batch_kernel: ->(columns, output) { columns.first.each { |value| output << value * 2_654_435_761 % 1_000_003 } })
  end

  # The native API binds bigint arrays directly. The Rust-binding Ruby harness
  # only has scalar streams, so add this generator locally to this explicit lane.
  module ArrayParameterStream
    def next_args
      args = []
      @params.each_with_index do |parameter, index|
        args << case parameter.generator
                when 'serial' then (value = @serials[index]; @serials[index] += 1; value)
                when 'int_uniform' then @prng.int_uniform(parameter.min, parameter.max)
                when 'int_window' then args[parameter.base] + @prng.int_uniform(parameter.off_min, parameter.off_max)
                when 'text' then @prng.text(parameter.min_len, parameter.max_len)
                when 'int_array'
                  length = @prng.int_uniform(parameter.min_len, parameter.max_len)
                  JedNative::ArrayValue.new(Array.new(length) { @prng.int_uniform(parameter.elem_min, parameter.elem_max) }, element_type: 'i64')
                else raise "unknown param gen #{parameter.generator}"
                end
      end
      args
    end
  end
  Bench::ParamStream.prepend(ArrayParameterStream)

  class Engine
    attr_reader :database
    def initialize(db, data_dir, dataset, scratch_dir = nil)
      @database, @db, @data_dir, @dataset, @scratch_dir = db, db.session, data_dir, dataset, scratch_dir
      @closed = false
    end

    def exec(sql) = @db.execute(sql)
    def prepare(sql) = (@statement = @db.prepare(sql))
    def exec_prepared(args) = @db.execute_prepared(@statement, *args)
    def query_int(sql) = @db.query(sql).rows.first.first
    def stored_fingerprint = Bench.read_sidecar(@data_dir, @dataset, 'jed')

    def query_prepared(args, sum)
      self.class.drain(@db.query_prepared(@statement, *args), sum)
    end

    def self.drain(result, sum)
      count = 0
      result.each do |row|
        count += 1
        if sum
          row.each do |value|
            case value
            when nil then sum.null
            when Integer then sum.int(value)
            when String then sum.text(value)
            else raise "unsupported checksum value #{value.class}"
            end
          end
          sum.end_row
        end
      end
      count
    end

    def close
      return if @closed

      @closed = true
      @db.close
      @database.close
    ensure
      FileUtils.remove_entry(@scratch_dir) if @scratch_dir && File.directory?(@scratch_dir)
    end
  end

  # Reader sessions share one file and page cache. MRI's global VM lock still
  # serializes Ruby execution; these threads measure that runtime honestly.
  def self.run_concurrent(config, workload, readers, data_dir, fingerprint)
    raise 'concurrent_read requires positive readers' unless readers.positive?
    engine = config[:open].call(data_dir, workload.dataset)
    sessions = Array.new(readers) { engine.database.read_session }
    stream = Bench::ParamStream.new(workload)
    warmup = Array.new(workload.warmup) { stream.next_args }
    measured = Array.new(workload.iterations) { stream.next_args }
    sql = workload.sql_for(config[:engine])
    sums = Array.new(readers) { Bench::Checksum.new }
    latencies = Array.new(readers) { [] }
    rows = Array.new(readers, 0)
    pass = lambda do |parameters, timed|
      threads = sessions.each_with_index.map do |session, index|
        base, extra = parameters.length.divmod(readers)
        first = base * index + [index, extra].min
        last = first + base + (index < extra ? 1 : 0)
        Thread.new do
          parameters[first...last].each do |args|
            start = Bench.now_ns if timed
            count = Engine.drain(session.query(sql, *args), timed ? sums[index] : nil)
            next unless timed
            latencies[index] << Bench.now_ns - start
            rows[index] += count
            if workload.expect_rows_per_iter && count != workload.expect_rows_per_iter
              raise "expected #{workload.expect_rows_per_iter} rows per iteration, got #{count}"
            end
          end
        end
      end
      errors = threads.filter_map do |thread|
        begin
          thread.value
          nil
        rescue Exception => error
          error
        end
      end
      raise errors.first unless errors.empty?
    end
    pass.call(warmup, false)
    started_at = Time.now.utc.strftime('%Y-%m-%dT%H:%M:%SZ')
    start = Bench.now_ns
    pass.call(measured, true)
    elapsed = Bench.now_ns - start
    combined = Bench::Checksum.new
    sums.each { |sum| combined.text(sum.hex) }
    all = latencies.flatten.sort
    result = Bench::ResultLine.new(workload.name, workload.dataset, workload.iterations,
      workload.warmup, elapsed, elapsed / workload.iterations, all.first,
      Bench.percentile(all, 50), Bench.percentile(all, 90), Bench.percentile(all, 99),
      rows.sum, combined.hex, fingerprint, started_at)
    result.define_singleton_method(:to_json_line) do |configuration|
      JSON.generate(JSON.parse(super(configuration)).merge('readers' => readers))
    end
    result
  ensure
    sessions&.each(&:close)
    engine&.close
  end

  def self.main
    unless ARGV.length.between?(3, 4)
      warn "usage: #{$PROGRAM_NAME} <corpus_dir> <data_dir> <out_path> [name_filter]"
      return 2
    end
    corpus_dir, data_dir, out_path, filter = ARGV
    filter ||= ''
    smoke = ENV['RUBY_NATIVE_BENCH_SMOKE'] == '1'
    timeout = Integer(ENV.fetch('RUBY_NATIVE_BENCH_TIMEOUT', '60'))
    source = File.expand_path('../../impl/ruby-native', __dir__)
    source_hash = NativeRubySource.digest(source)
    fingerprint = Bench.corpus_fingerprint(corpus_dir)
    handles = []
    workload_data = TomlRB.load_file(File.join(corpus_dir, 'benchmarks.toml')).fetch('bench').to_h do |entry|
      [entry.fetch('name'), entry]
    end
    synchronous = :on
    config = { engine: 'jed', lang: 'ruby-native', variant: 'core', open: lambda do |dir, dataset|
      scratch = dataset == 'scratch' ? Dir.mktmpdir('scratch-ruby-native-', dir) : nil
      path = scratch ? File.join(scratch, 'scratch.jed') : File.join(dir, "#{dataset}.jed")
      options = { environment: environment, synchronous: synchronous }
      db = scratch ? JedNative.create(path, **options) : JedNative.open(path, **options)
      engine = Engine.new(db, dir, dataset, scratch)
      handles << engine
      engine
    end }
    outcomes = []
    File.open(out_path, 'w') do |output|
      Bench.load_corpus(corpus_dir).each do |workload|
        next unless workload.runs_on?('jed') && workload.name.include?(filter)
        workload.params = workload.params.each_with_index.map do |parameter, index|
          raw = workload_data.fetch(workload.name).fetch('param', []).fetch(index)
          Parameter.new(*parameter.to_a, raw['elem_min'], raw['elem_max'])
        end

        synchronous = workload.relaxed ? :off : :on
        begin
          warn "jed/ruby-native/core: #{workload.name} (#{workload.dataset}) ..."
          if workload.dataset != 'scratch' && Bench.read_sidecar(data_dir, workload.dataset, 'jed') != fingerprint
            raise "stale or missing benchmark data for #{workload.dataset}/jed: regenerate with the current bench-setup"
          end
          execution = workload.dup
          execution.warmup, execution.iterations = 0, 1 if smoke
          run = lambda do
            if workload.kind == 'concurrent_read'
              run_concurrent(config, execution, workload_data.fetch(workload.name).fetch('readers'), data_dir, fingerprint)
            else
              Bench.run_one(config, execution, corpus_dir, data_dir, fingerprint)
            end
          end
          result = smoke ? Timeout.timeout(timeout) { run.call } : run.call
          if smoke
            outcomes << { bench: workload.name, status: 'smoke_pass', checksum: result.checksum,
                          rows: result.rows_total, iterations: 1, batch: workload.batch }
          else
            output.puts(result.to_json_line(config))
            output.flush
            outcomes << { bench: workload.name, status: 'completed' }
          end
        rescue StandardError, NotImplementedError => error
          outcomes << { bench: workload.name, status: 'failed', reason: error.message }
          warn "ruby-native: #{workload.name}: FAILED: #{error.message}"
        ensure
          handles.each(&:close)
          handles.clear
          File.write("#{out_path}.status.json", JSON.pretty_generate(outcomes) + "\n")
        end
      end
    end
    File.write("#{out_path}.status.json", JSON.pretty_generate(outcomes) + "\n")
    File.write("#{out_path}.metadata.json", JSON.pretty_generate(
      mode: smoke ? 'single_execution_smoke_no_timings' : 'standard_benchmark',
      ruby: RUBY_DESCRIPTION, source_sha256: source_hash,
      source_sha256_after: NativeRubySource.digest(source),
      dataset_fingerprint: fingerprint,
      workload_fingerprint: Digest::SHA256.file(File.join(corpus_dir, 'benchmarks.toml')).hexdigest,
      timeout_seconds_per_workload: smoke ? timeout : nil
    ) + "\n")
    counts = outcomes.group_by { |outcome| outcome[:status] }.transform_values(&:length)
    warn "ruby-native benchmark outcomes: #{counts}; details: #{out_path}.status.json"
    outcomes.all? { |outcome| %w[completed smoke_pass].include?(outcome[:status]) } ? 0 : 1
  rescue StandardError => error
    warn "ruby-native benchmark failed: #{error.message}"
    1
  end
end

exit NativeRubyBench.main
