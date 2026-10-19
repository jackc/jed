#!/usr/bin/env ruby
# frozen_string_literal: true

# Diagnostic kernel comparison, not a SQL or durable-workload benchmark. Choose
# the source explicitly to compare an archived Ruby kernel with stdlib Zlib:
# RUBY_NATIVE_SOURCE=/path/to/impl/ruby-native mise run ruby-native:bounded -- \
#   ruby --yjit bench/ruby-native/checksum_probe.rb /path/to/results.json
# The original lookup-followup measurements used the library from c3ba8e69.
require 'json'
require 'zlib'
require_relative 'source'

abort 'usage: checksum_probe.rb OUTPUT.json' unless ARGV.length == 1
root = File.expand_path('../..', __dir__)
source = NativeRubySource.resolve(root, phase: 'checksum-probe')
source_hash = NativeRubySource.digest(source)
require File.join(source, 'lib/jed_native/storage')
codec = JedNative::FileFormat.new
bytes = (0..255).to_a.pack('C*') * 32
operations = {
  source_crc32_8k: -> { codec.crc32(bytes) },
  zlib_crc32_8k: -> { Zlib.crc32(bytes) },
  source_crc64_8k: -> { codec.crc64(bytes) },
  zlib_page_concat_8k: -> { Zlib.crc32(bytes.byteslice(0, 12) + bytes.byteslice(16, bytes.bytesize - 16)) },
  zlib_page_stream_8k: -> { Zlib.crc32(bytes.byteslice(16, bytes.bytesize - 16), Zlib.crc32(bytes.byteslice(0, 12))) },
  zlib_meta_concat_8k: -> { Zlib.crc32(bytes.byteslice(0, 32) + bytes.byteslice(36, bytes.bytesize - 36)) },
  zlib_meta_stream_8k: -> { Zlib.crc32(bytes.byteslice(36, bytes.bytesize - 36), Zlib.crc32(bytes.byteslice(0, 32))) }
}
raise 'CRC32 implementation disagreement' unless operations[:source_crc32_8k].call == operations[:zlib_crc32_8k].call
raise 'CRC32 page-span disagreement' unless operations[:zlib_page_concat_8k].call == operations[:zlib_page_stream_8k].call
raise 'CRC32 meta-span disagreement' unless operations[:zlib_meta_concat_8k].call == operations[:zlib_meta_stream_8k].call

repetitions = 7
results = operations.to_h do |name, operation|
  iterations = name.to_s.start_with?('source_') ? 1000 : 10000
  1000.times { operation.call }
  samples = Array.new(repetitions) do
    before = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)
    iterations.times { operation.call }
    Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond) - before
  end
  [name, { iterations: iterations, samples_ns: samples,
           median_ns: samples.sort[repetitions / 2].fdiv(iterations), checksum: operation.call.to_s(16) }]
end
raise 'Ruby source changed during measurement' unless NativeRubySource.digest(source) == source_hash
report = { phase: 'checksum-diagnostic', runtime: RUBY_DESCRIPTION,
           yjit: defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?,
           zjit: defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?,
           zlib_gem: Zlib::VERSION, zlib_runtime: Zlib.zlib_version,
           source: source, source_sha256: source_hash, repetitions: repetitions,
           warmup_iterations_per_operation: 1000, results: results }
File.write(ARGV.fetch(0), JSON.pretty_generate(report) + "\n")
puts JSON.pretty_generate(report)
