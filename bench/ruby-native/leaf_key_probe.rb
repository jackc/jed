#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit synthetic kernel: four field reads from one hot decoded leaf. This
# isolates cache-key hashing; it is not a database/query performance result.
require 'json'
require 'rbconfig'
require_relative 'source'

source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', '../../impl/ruby-native'), __dir__)
before = NativeRubySource.digest(source)
require File.join(source, 'lib/jed_native')

class StructuralLeafPool < JedNative::LeafPool
  def leaf(index, columns)
    decoded_cache.fetch([index, columns]) do
      raw = page(index)
      keys, regions = @codec.parse_pax_leaf(raw[:payload], raw[:item_count], columns)
      raise 'unordered keys' unless keys.each_cons(2).all? { |a, b| a < b }
      { keys: keys, regions: regions, count: raw[:item_count] }
    end
  end
end

class LeafKeyProbeCodec
  def read_page(*) = { type: JedNative::FileFormat::PAGE_LEAF, payload: ''.b, item_count: 1 }
  def parse_pax_leaf(*) = [["key".b], [41, 7, 900, 'note']]
  def leaf_value(region, *) = region
end

columns = [['id', 'i64'], ['customer_id', 'i32'], ['amount', 'i32'], ['note', 'text']].map do |name, type|
  { name: name, type: type, not_null: name == 'id', default: nil, collation: type == 'text' ? 'C' : nil }
end
results = { structural_schema: StructuralLeafPool, identity_schema: JedNative::LeafPool }.map do |name, klass|
  pool = klass.new(LeafKeyProbeCodec.new, ''.b, 8192, 1)
  leaf = JedNative::PagedLeaf.new(pool, 3, columns)
  work = lambda do |iterations|
    checksum = 0
    iterations.times do
      row = JedNative::PagedRow.new(leaf, 0)
      checksum += row[0] + row[1] + row[2] + row[3].bytesize
    end
    raise 'wrong result' unless checksum == 952 * iterations
  end
  work.call(5_000)
  samples = 5.times.map do
    allocated = GC.stat(:total_allocated_objects)
    start = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)
    work.call(20_000)
    { ns_per_op: (Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond) - start) / 20_000.0,
      allocations_per_op: (GC.stat(:total_allocated_objects) - allocated) / 20_000.0 }
  end
  [name, { samples: samples, median_ns_per_op: samples.map { |sample| sample[:ns_per_op] }.sort[2], cache_entries: pool.cached_leaves }]
end.to_h
after = NativeRubySource.digest(source)
raise 'source changed during probe' unless before == after
report = { mode: 'synthetic_leaf_key_kernel', runtime: RUBY_DESCRIPTION, ruby_executable: RbConfig.ruby,
  source: source, source_sha256: before, final_source_sha256: after,
  script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
  notes: ['Four PagedRow field reads over a fake codec and one cached leaf; no SQL, file I/O, or decoding.',
          'StructuralLeafPool reproduces the previous structural schema key. Identity mode uses the current core.',
          'Sequential lanes, 5000 warmup operations, 5 batches of 20000 measured operations.'],
  results: results }
puts JSON.pretty_generate(report)
