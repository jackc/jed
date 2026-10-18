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
    def initialize(db, data_dir, dataset, scratch_dir = nil)
      @db, @data_dir, @dataset, @scratch_dir = db, data_dir, dataset, scratch_dir
      @closed = false
    end

    def exec(sql) = @db.execute(sql)
    def prepare(sql) = (@statement = @db.prepare(sql))
    def exec_prepared(args) = @db.execute_prepared(@statement, *args)
    def query_int(sql) = @db.query(sql).rows.first.first
    def stored_fingerprint = Bench.read_sidecar(@data_dir, @dataset, 'jed')

    def query_prepared(args, sum)
      result = @db.query_prepared(@statement, *args)
      if sum
        result.rows.each do |row|
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
      result.rows.length
    end

    def close
      return if @closed

      @closed = true
      @db.close
    ensure
      FileUtils.remove_entry(@scratch_dir) if @scratch_dir && File.directory?(@scratch_dir)
    end
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
    parameter_data = TomlRB.load_file(File.join(corpus_dir, 'benchmarks.toml')).fetch('bench').to_h do |entry|
      [entry.fetch('name'), entry.fetch('param', [])]
    end
    config = { engine: 'jed', lang: 'ruby-native', variant: 'core', open: lambda do |dir, dataset|
      scratch = dataset == 'scratch' ? Dir.mktmpdir('scratch-ruby-native-', dir) : nil
      path = scratch ? File.join(scratch, 'scratch.jed') : File.join(dir, "#{dataset}.jed")
      db = scratch ? JedNative.create(path) : JedNative.open(path)
      engine = Engine.new(db, dir, dataset, scratch)
      handles << engine
      engine
    end }
    outcomes = []
    File.open(out_path, 'w') do |output|
      Bench.load_corpus(corpus_dir).each do |workload|
        next unless workload.runs_on?('jed') && workload.name.include?(filter)
        workload.params = workload.params.each_with_index.map do |parameter, index|
          raw = parameter_data.fetch(workload.name).fetch(index)
          Parameter.new(*parameter.to_a, raw['elem_min'], raw['elem_max'])
        end

        unsupported = if workload.kind == 'concurrent_read'
                        'independent concurrent sessions are not implemented'
                      elsif workload.relaxed
                        'batched synchronous=off durability is not implemented'
                      elsif workload.host_functions
                        'host-defined function registration is not implemented'
                      end
        if unsupported
          outcomes << { bench: workload.name, status: 'unsupported', reason: unsupported }
          warn "ruby-native: #{workload.name}: UNSUPPORTED: #{unsupported}"
          next
        end
        begin
          warn "jed/ruby-native/core: #{workload.name} (#{workload.dataset}) ..."
          if workload.dataset != 'scratch' && Bench.read_sidecar(data_dir, workload.dataset, 'jed') != fingerprint
            raise "stale or missing benchmark data for #{workload.dataset}/jed: regenerate with the current bench-setup"
          end
          execution = workload.dup
          execution.warmup, execution.iterations = 0, 1 if smoke
          result = if smoke
                     Timeout.timeout(timeout) { Bench.run_one(config, execution, corpus_dir, data_dir, fingerprint) }
                   else
                     Bench.run_one(config, execution, corpus_dir, data_dir, fingerprint)
                   end
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
