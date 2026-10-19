# frozen_string_literal: true
require 'json'
require File.join(ENV.fetch('RUBY_NATIVE_SOURCE', Dir.pwd + '/impl/ruby-native'), 'lib/jed_native')
class LeafKeyKernelCodec
  def read_page(*)
    { type: JedNative::FileFormat::PAGE_LEAF, payload: ''.b, item_count: 1 }
  end
  def parse_pax_leaf(*)
    [["key".b], [41, 7, 900, 'note']]
  end
  def leaf_value(region, *) = region
end
columns = [['id','i64'],['customer_id','i32'],['amount','i32'],['note','text']].map do |name,type|
  { name: name, type: type, not_null: name == 'id', default: nil, collation: type == 'text' ? 'C' : nil }
end
pool = JedNative::LeafPool.new(LeafKeyKernelCodec.new, ''.b, 8192, 1)
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
  before = GC.stat(:total_allocated_objects)
  started = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)
  work.call(20_000)
  { ns_per_op: (Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond) - started) / 20_000.0,
    allocations_per_op: (GC.stat(:total_allocated_objects) - before) / 20_000.0 }
end
puts JSON.pretty_generate(runtime: RUBY_DESCRIPTION, samples: samples,
 median_ns_per_op: samples.map { |sample| sample[:ns_per_op] }.sort[2],
 cache_entries: pool.cached_leaves)
