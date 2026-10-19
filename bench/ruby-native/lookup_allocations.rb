#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit allocation diagnostic. Tracing and disabled GC deliberately distort
# execution; only the untraced checksum replay below carries timing results.
require 'json'
require 'objspace'
require 'rbconfig'
require_relative 'source'
require_relative '../ruby/lib/bench'

abort 'usage: lookup_allocations.rb REPORT' unless ARGV.length == 1
root = File.expand_path('../..', __dir__)
source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(root, 'impl/ruby-native')))
before = NativeRubySource.digest(source)
require File.join(source, 'lib/jed_native')
workload = Bench.load_corpus(File.join(root, 'bench/corpus')).find { |entry| entry.name == 'point_lookup_pk' }
stream = Bench::ParamStream.new(workload)
parameters = Array.new(100) { stream.next_args }
database = JedNative.open(File.join(root, '.dev/ruby-native-bench-data/large.jed'), locking: :exclusive)
session = database.session
prepared = session.prepare(workload.sql_for('jed'))
rows = parameters.map { |args| session.query_prepared(prepared, *args).rows.first }
add = lambda do |checksum, row|
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
query = lambda do |checksum|
  parameters.each do |args|
    count = 0
    session.query_prepared(prepared, *args).each do |row|
      raise 'wrong row' unless row[0] == args[0]
      count += 1
      add.call(checksum, row) if checksum
    end
    raise 'wrong row count' unless count == 1
  end
end
10.times { query.call(Bench::Checksum.new) }
phases = {
  engine_only: -> { query.call(nil) },
  checksum_only: -> { sum = Bench::Checksum.new; rows.each { |row| add.call(sum, row) }; sum.hex },
  combined: -> { sum = Bench::Checksum.new; query.call(sum); sum.hex }
}
profiles = phases.to_h do |name, work|
  GC.start
  ObjectSpace.trace_object_allocations_clear
  previously_disabled = GC.disable
  allocated = GC.stat(:total_allocated_objects)
  ObjectSpace.trace_object_allocations_start
  begin
    checksum = work.call
  ensure
    ObjectSpace.trace_object_allocations_stop
  end
  total = GC.stat(:total_allocated_objects) - allocated
  sites = Hash.new { |hash, key| hash[key] = { count: 0, bytes: 0 } }
  ObjectSpace.each_object do |object|
    path = ObjectSpace.allocation_sourcefile(object)
    next unless path
    # Keep archived/external source snapshots in the same core category as the
    # working tree, while retaining repository-relative paths in the report.
    path = if path.start_with?(File.join(source, 'lib/'))
             'impl/ruby-native/lib/' + path.delete_prefix(File.join(source, 'lib/'))
           else
             path.delete_prefix(root + '/')
           end
    line = ObjectSpace.allocation_sourceline(object)
    site = sites[[path, line, object.class.name]]
    site[:count] += 1
    site[:bytes] += ObjectSpace.memsize_of(object)
  end
  ObjectSpace.trace_object_allocations_clear
  GC.enable unless previously_disabled
  grouped = sites.group_by do |(path, _, _), _|
    path.start_with?('impl/ruby-native/lib/') ? :core : path == 'bench/ruby/lib/bench.rb' ? :benchmark_checksum : :other
  end.transform_values do |items|
    { allocations_per_query: items.sum { |_, value| value[:count] } / 100.0,
      bytes_per_query: items.sum { |_, value| value[:bytes] } / 100.0 }
  end
  [name, { operations: 100, total_allocations_per_query: total / 100.0, checksum: checksum.is_a?(String) ? checksum : nil,
    grouped: grouped, sites: sites.sort_by { |_, data| -data[:count] }.first(45).map { |(path, line, klass), data| data.merge(path: path, line: line, class: klass) } }]
end
session.close
database.close
GC.start
samples = 5.times.map do
  sum = Bench::Checksum.new
  allocated = GC.stat(:total_allocated_objects)
  start = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)
  100.times { rows.each { |row| add.call(sum, row) } }
  elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond) - start
  { ns_per_row: elapsed / 10_000.0, allocations_per_row: (GC.stat(:total_allocated_objects) - allocated) / 10_000.0, checksum: sum.hex }
end
after = NativeRubySource.digest(source)
raise 'source changed during probe' unless before == after
report = { mode: 'primary_key_allocation_diagnostic', runtime: RUBY_DESCRIPTION, ruby_executable: RbConfig.ruby,
  source: source, source_sha256: before, final_source_sha256: after, sql: workload.sql_for('jed'), seed: workload.seed,
  script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
  notes: ['100 warmed canonical parameters; allocation tracing with GC disabled for one100-operation phase at a time.',
          'Allocation-phase timings are deliberately not reported. No decoded leaf misses occurred after warmup.',
          'Checksum replay times the unchanged canonical Ruby FNV-1a implementation over precollected results; no SQL is executed.'],
  profiles: profiles, checksum_replay_samples: samples }
File.write(ARGV.first, JSON.pretty_generate(report) + "\n")
puts JSON.pretty_generate(report.except(:profiles).merge(allocation_groups: profiles.transform_values { |entry| entry[:grouped] }))
