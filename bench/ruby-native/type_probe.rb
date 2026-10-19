#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit microkernel for already-resolved type names and integer admission.
# This is not a SQL/database benchmark.
require 'json'
require 'rbconfig'
require_relative 'source'
source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', '../../impl/ruby-native'), __dir__)
file = File.join(source, 'lib/jed_native/types.rb')
before = Digest::SHA256.file(file).hexdigest
require File.join(source, 'lib/jed_native')
work = lambda do |count|
  count.times do
    %w[i16 i32 i64 text].each do |type|
      raise 'normalization changed' unless JedNative::Types.normalize(type) == type && JedNative::Types.base(type) == type
    end
    %w[i16 i32 i64].each { |type| raise 'integer admission changed' unless JedNative::Types.check_integer(123, type) == 123 }
  end
end
work.call(3_000)
samples = 5.times.map do
  allocated = GC.stat(:total_allocated_objects)
  start = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)
  work.call(10_000)
  { ns_per_op: (Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond) - start) / 10_000.0,
    allocations_per_op: (GC.stat(:total_allocated_objects) - allocated) / 10_000.0 }
end
after = Digest::SHA256.file(file).hexdigest
raise 'type source changed during probe' unless before == after
puts JSON.pretty_generate(mode: 'resolved_type_microkernel', runtime: RUBY_DESCRIPTION, ruby_executable: RbConfig.ruby,
  source: source, types_sha256: before, final_types_sha256: after, script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
  notes: ['Each operation normalizes and extracts the base of4 canonical scalar types, then checks3 integer values.',
          'No SQL or file I/O; 3000 warmup operations followed by5 batches of10000 operations.'],
  samples: samples, median_ns_per_op: samples.map { |entry| entry[:ns_per_op] }.sort[2])
