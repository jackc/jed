#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit diagnostic, separate from canonical benchmark timings. TracePoint
# runs after all timed batches because tracing changes JIT execution.
require 'json'
require 'optparse'
require 'rbconfig'
require 'time'
require_relative 'source'
require_relative '../ruby/lib/bench'

options = { warmup: 50_000, iterations: 5_000, repetitions: 5, trace: 100 }
OptionParser.new do |cli|
  cli.banner = 'Usage: mise run ruby-native:bounded -- ruby [--yjit|--zjit] bench/ruby-native/lookup_profile.rb --report PATH'
  cli.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
  cli.on('--warmup COUNT', Integer) { |value| options[:warmup] = value }
  cli.on('--iterations COUNT', Integer) { |value| options[:iterations] = value }
  cli.on('--repetitions COUNT', Integer) { |value| options[:repetitions] = value }
  cli.on('--trace COUNT', Integer) { |value| options[:trace] = value }
end.parse!
abort 'a report path and positive iteration counts are required' unless options[:report] && options.except(:report).values.all?(&:positive?) && ARGV.empty?

root = File.expand_path('../..', __dir__)
source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(root, 'impl/ruby-native')))
source_sha256 = NativeRubySource.digest(source)
require File.join(source, 'lib/jed_native')
workload = Bench.load_corpus(File.join(root, 'bench/corpus')).find { |entry| entry.name == 'point_lookup_pk' }
stream = Bench::ParamStream.new(workload)
parameters = Array.new(options[:warmup] + options[:iterations] * options[:repetitions]) { stream.next_args }
dataset = File.join(root, '.dev/ruby-native-bench-data/large.jed')
database = JedNative.open(dataset, locking: :exclusive)
session = database.session
prepared = session.prepare(workload.sql_for('jed'))
clock = -> { Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond) }
drain = lambda do |result, args, checksum|
  count = 0
  result.each do |row|
    raise 'incorrect point lookup' unless row[0] == args[0]
    count += 1
    if checksum
      row.each do |value|
        case value
        when nil then checksum.null
        when Integer then checksum.int(value)
        when String then checksum.text(value)
        else raise "unsupported checksum value #{value.class}"
        end
      end
      checksum.end_row
    end
  end
  raise 'incorrect point lookup row count' unless count == 1
end
options[:warmup].times { |index| drain.call(session.query_prepared(prepared, *parameters[index]), parameters[index], nil) }
GC.start
samples = options[:repetitions].times.map do |batch|
  checksum = Bench::Checksum.new
  setup_ns = drain_ns = 0
  before = GC.stat
  start = clock.call
  options[:iterations].times do |index|
    args = parameters[options[:warmup] + batch * options[:iterations] + index]
    begin_ns = clock.call
    result = session.query_prepared(prepared, *args)
    ready_ns = clock.call
    drain.call(result, args, checksum)
    finish_ns = clock.call
    setup_ns += ready_ns - begin_ns
    drain_ns += finish_ns - ready_ns
  end
  elapsed = clock.call - start
  after = GC.stat
  { total_ns: elapsed, ns_per_query: elapsed.fdiv(options[:iterations]),
    setup_ns_per_query: setup_ns.fdiv(options[:iterations]),
    drain_ns_per_query: drain_ns.fdiv(options[:iterations]),
    allocations_per_query: (after[:total_allocated_objects] - before[:total_allocated_objects]).fdiv(options[:iterations]),
    gc_count: after[:count] - before[:count], checksum: checksum.hex }
end

calls = Hash.new(0)
trace = TracePoint.new(:call) do |event|
  next unless event.path.start_with?(File.join(source, 'lib/'))
  calls["#{event.defined_class}##{event.method_id}"] += 1
end
trace.enable do
  options[:trace].times do |index|
    args = parameters[index % parameters.length]
    drain.call(session.query_prepared(prepared, *args), args, nil)
  end
end
session.close
database.close
final_sha256 = NativeRubySource.digest(source)
raise 'source changed during profiling' unless source_sha256 == final_sha256
jit = if defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled? then 'yjit'
      elsif defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled? then 'zjit'
      else 'interpreter'
      end
report = { schema: 1, mode: 'primary_key_diagnostic', created_at: Time.now.utc.iso8601,
  ruby: RUBY_DESCRIPTION, ruby_executable: RbConfig.ruby, ruby_executable_sha256: Digest::SHA256.file(RbConfig.ruby).hexdigest,
  jit: jit, source: source, source_sha256: source_sha256, final_source_sha256: final_sha256,
  script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
  dataset: { path: dataset, bytes: File.size(dataset), sha256: Digest::SHA256.file(dataset).hexdigest },
  sql: workload.sql_for('jed'), seed: workload.seed, locking: 'exclusive', options: options,
  notes: ['Canonical SQL/data/parameter stream; shorter diagnostic batches with additional timer and result checks.',
          'Setup includes lazy cursor construction; drain includes execution, host row conversion, and benchmark checksum.',
          'TracePoint runs after timing. Call counts are diagnostic; traced timings are never used as performance measurements.'],
  samples: samples,
  calls_per_query: calls.sort_by { |name, count| [-count, name] }.to_h.transform_values { |count| count.fdiv(options[:trace]) },
  crc_calls_per_query: %w[crc32 crc64 page_crc meta_crc].to_h { |method| [method, calls.select { |name, _| name.end_with?("##{method}") }.values.sum.fdiv(options[:trace])] }
}
FileUtils.mkdir_p(File.dirname(options[:report]))
File.write(options[:report], JSON.pretty_generate(report) + "\n")
puts JSON.pretty_generate(report.except(:calls_per_query, :dataset))
