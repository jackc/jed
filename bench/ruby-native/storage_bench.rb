#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit storage-kernel experiment, separate from SQL/end-to-end benchmarks.
# The durable cases include full snapshot encoding, page writes, and fsync on
# this filesystem; they do not include SQL parsing or query execution.
require 'json'
require 'digest'
require 'fileutils'
require 'tmpdir'
require 'time'
require 'open3'
require_relative 'source'

root = File.expand_path('../..', __dir__)
phase = ENV.fetch('RUBY_NATIVE_PHASE', 'baseline')
raise 'invalid phase name' unless phase.match?(/\A[a-zA-Z0-9_-]+\z/)
source = NativeRubySource.resolve(root, phase: phase)
require File.join(source, 'lib/jed_native/storage')

repetitions = 5
results = {}
codec = JedNative::FileFormat.new
bytes = (0..255).to_a.pack('C*') * 32
%i[crc32 crc64].each do |operation|
  iterations = 40
  5.times { codec.public_send(operation, bytes) }
  samples = Array.new(repetitions) do
    start = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)
    iterations.times { codec.public_send(operation, bytes) }
    Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond) - start
  end
  results["#{operation}_8k"] = { iterations: iterations, samples_ns: samples,
                               checksum: codec.public_send(operation, bytes).to_s(16) }
end

parent = File.join(root, '.dev/ruby-native-storage-bench')
FileUtils.mkdir_p(parent)
[1, 100].each do |count|
  Dir.mktmpdir('durable-', parent) do |dir|
    storage = JedNative::Storage.new(path: File.join(dir, 'test.jed'), create: true)
    columns = [codec.col('id', 'i32', pk: true), codec.col('v', 'text')]
    rows = Array.new(count) { |i| [i, "value-#{i}"] }
    state = { tables: [{ name: 't', columns: columns, rows: rows }] }
    storage.commit(state)
    counter = 0
    commit = lambda do
      counter += 1
      rows[counter % count][1] = "updated-#{counter}"
      storage.commit(state)
    end
    3.times { commit.call }
    iterations = 10
    samples = Array.new(repetitions) do
      start = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)
      iterations.times { commit.call }
      Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond) - start
    end
    path = storage.path
    metadata = storage.meta.dup
    storage.close
    reopened = JedNative::Storage.new(path: path)
    raise 'durable benchmark rows did not survive reopen' unless reopened.load[:tables].first[:rows] == rows
    reopened.close
    results["durable_full_snapshot_#{count}_rows"] = { iterations: iterations, samples_ns: samples,
                                                       final_txid: metadata[:txid], live_pages: metadata[:live],
                                                       physical_pages: metadata[:page_count] }
  end
end

filesystem, = Open3.capture2('stat', '-f', '-c', '%T', parent)
report = { schema: 1, phase: phase, created_at: Time.now.utc.iso8601, runtime: RUBY_DESCRIPTION,
           platform: RUBY_PLATFORM, filesystem: filesystem.strip, repetitions: repetitions,
           ruby_source: source,
           ruby_source_sha256: NativeRubySource.digest(source),
           results: results }
out = File.join(__dir__, 'results', "#{phase}-storage.json")
FileUtils.mkdir_p(File.dirname(out))
File.write(out, JSON.pretty_generate(report) + "\n")
results.each do |name, result|
  median = result[:samples_ns].sort[repetitions / 2].fdiv(result[:iterations])
  puts "#{name}: #{format('%.1f', median)} ns/op"
end
puts "storage benchmark artifact: #{out}"
