#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit diagnostic: all timing completes before TracePoint/ObjectSpace run.
# Use the unchanged comparison harness for published cross-core measurements.
require 'json'
require 'objspace'
require 'optparse'
require 'rbconfig'
require 'time'
require_relative 'source'

options = { api: 'both', trace: 3 }
OptionParser.new do |cli|
  cli.on('--report PATH') { |path| options[:report] = File.expand_path(path) }
  cli.on('--api NAME', %w[query execute both]) { |api| options[:api] = api }
  cli.on('--trace COUNT', Integer) { |count| options[:trace] = count }
end.parse!
abort 'a report path and positive trace count are required' unless options[:report] && options[:trace].positive? && ARGV.empty?
root = File.expand_path('../..', __dir__)
source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(root, 'impl/ruby-native')))
source_hash = NativeRubySource.digest(source)
require File.join(source, 'lib/jed_native')
plan_path = File.expand_path(ENV.fetch('RUBY_NATIVE_COMPARE_PLAN', File.join(__dir__, 'compare/plan.json')))
plan = JSON.parse(File.read(plan_path))
database = JedNative.memory
session = database.session
session.execute('CREATE TABLE items (id i32 PRIMARY KEY, bucket i32, score i32, label text)')
values = plan.fetch('rows').times.map { |i| "(#{i},#{i % 10},#{(i * 37) % 10000},'row#{i}')" }.join(',')
session.execute("INSERT INTO items VALUES #{values}")
apis = options[:api] == 'both' ? %w[query execute] : [options[:api]]
jobs = apis.flat_map do |api|
  plan.fetch('workloads').map do |workload|
    count = [workload['iterations'], workload['warmup'], options[:trace]].max
    params = Array.new(count) do |i|
      workload['span'].zero? ? [] : [workload['offset'] + (i * workload['step']) % workload['span']]
    end
    { api: api, workload: workload, prepared: session.prepare(workload['sql']), params: params,
      method: session.method("#{api}_prepared") }
  end
end
clock = -> { Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond) }
drain = lambda do |job, index|
  result = job[:method].call(job[:prepared], *job[:params][index])
  result.rows
ensure
  result&.close
end
results = jobs.map do |job|
  workload = job[:workload]
  workload['warmup'].times { |i| drain.call(job, i) }
  samples = plan['repetitions'].times.map do
    count = 0
    allocated = GC.stat(:total_allocated_objects)
    gc = GC.stat(:count)
    started = clock.call
    workload['iterations'].times { |i| count += drain.call(job, i).length }
    elapsed = clock.call - started
    { ns_per_query: elapsed.fdiv(workload['iterations']), rows: count,
      allocations_per_query: (GC.stat(:total_allocated_objects) - allocated).fdiv(workload['iterations']),
      gc_count: GC.stat(:count) - gc }
  end
  { api: job[:api], workload: workload['name'], sql: workload['sql'], samples: samples }
end

jobs.each_with_index do |job, index|
  calls = Hash.new(0)
  trace = TracePoint.new(:call) do |event|
    next unless event.path.start_with?(File.join(source, 'lib/'))
    calls["#{event.defined_class}##{event.method_id}"] += 1
  end
  trace.enable { options[:trace].times { |i| drain.call(job, i) } }
  results[index][:calls_per_query] = calls.sort_by { |name, count| [-count, name] }.to_h.transform_values { |count| count.fdiv(options[:trace]) }

  GC.start
  ObjectSpace.trace_object_allocations_clear
  disabled = GC.disable
  ObjectSpace.trace_object_allocations_start
  begin
    rows = drain.call(job, 0)
  ensure
    ObjectSpace.trace_object_allocations_stop
  end
  sites = Hash.new(0)
  ObjectSpace.each_object do |object|
    path = ObjectSpace.allocation_sourcefile(object)
    next unless path&.start_with?(File.join(source, 'lib/'))
    path = path.delete_prefix(source + '/')
    sites[[path, ObjectSpace.allocation_sourceline(object), object.class.name]] += 1
  end
  results[index][:first_answer_sha256] = Digest::SHA256.hexdigest(JSON.generate(rows))
  results[index][:allocation_sites] = sites.sort_by { |site, count| [-count, site.join(':')] }.first(35).map do |(path, line, klass), count|
    { path: path, line: line, class: klass, count: count }
  end
  ObjectSpace.trace_object_allocations_clear
  GC.enable unless disabled
end
session.close
database.close
after = NativeRubySource.digest(source)
raise 'source changed during profiling' unless after == source_hash
report = { schema: 1, mode: 'query_shape_diagnostic', created_at: Time.now.utc.iso8601,
  ruby: RUBY_DESCRIPTION, ruby_executable: RbConfig.ruby,
  source: source, source_sha256: source_hash, final_source_sha256: after,
  script_sha256: Digest::SHA256.file(__FILE__).hexdigest, plan_path: plan_path, plan_sha256: Digest::SHA256.file(plan_path).hexdigest,
  options: options, notes: ['Canonical memory-comparison SQL/data/parameters and batch counts; diagnostic, not a cross-core benchmark.',
    'All timing precedes tracing. Allocation tracing disables GC for one query at a time; traced time is never reported.',
    'Core allocation sites exclude Ruby internal objects. Answer digests compare query and execute paths on the first parameter.'],
  results: results }
FileUtils.mkdir_p(File.dirname(options[:report]))
File.write(options[:report], JSON.pretty_generate(report) + "\n")
results.each do |result|
  samples = result[:samples]
  puts "#{result[:api]} #{result[:workload]}: #{samples.map { |sample| sample[:ns_per_query] }.sort[2].round(1)}ns; #{samples.map { |sample| sample[:allocations_per_query] }.sort[2].round(1)} allocations"
end
