#!/usr/bin/env ruby
# frozen_string_literal: true

# bench_jed.rb benchmarks the jed Ruby GEM (spec/design/benchmarks.md §6/§7) — engine=jed,
# lang=ruby, variant=wrap. It drives the same corpus the Rust core bench (jed/rust/core) runs,
# through the gem's public API, so the per-bench `jed/ruby/wrap` − `jed/rust/core` delta is the
# binding overhead: the FFI round-trip + result marshalling + value coercion + Ruby object
# allocation. NOTE: the gem has no prepared-statement API, so `query_prepared` re-parses the SQL
# each call (the core's `prepare` parses once) — that per-call parse is included in the delta. A
# gem prepared-statement API would isolate the pure FFI tax (a follow-on, ruby.md §6).

$LOAD_PATH.unshift(File.expand_path("lib", __dir__))
$LOAD_PATH.unshift(File.expand_path("../../impl/ruby/lib", __dir__))

require "fileutils"
require "bench"
require "jed"

# Wraps an open gem handle as the harness Engine. Reads exactly like bench_jed.rs / bench-jed.ts.
class JedEngine
  def initialize(db, data_dir, dataset, scratch_dir)
    @db = db
    @data_dir = data_dir
    @dataset = dataset
    @scratch_dir = scratch_dir
    @sql = nil
  end

  def exec(sql) = @db.execute(sql)

  # The gem exposes no prepared statement; stash the SQL and re-issue it each call (the per-call
  # parse is a documented part of the measured overhead).
  def prepare(sql)
    @sql = sql
  end

  def query_prepared(args, sum)
    result = @db.query(@sql, *args)
    n = 0
    result.each do |row|
      n += 1
      next unless sum

      row.each { |v| checksum_value(sum, v) }
      sum.end_row
    end
    n
  end

  def exec_prepared(args) = @db.execute(@sql, *args)

  def query_int(sql) = @db.query(sql).first[0]

  def stored_fingerprint = Bench.read_sidecar(@data_dir, @dataset, "jed")

  def close
    @db.close
    FileUtils.remove_entry(@scratch_dir) if @scratch_dir
  end

  private

  # The corpus's query columns are all int/text (datasets.toml), so the gem coerces every value to
  # Integer/String/nil — checksummed exactly as the core renders Value::Int/Text/Null, giving a
  # byte-identical cross-engine answer. Anything else is a corpus the gem bench can't checksum.
  def checksum_value(sum, value)
    case value
    when nil then sum.null
    when Integer then sum.int(value)
    when String then sum.text(value)
    else raise "unexpected result value #{value.inspect} (#{value.class}) — not int/text/null"
    end
  end
end

open_engine = lambda do |data_dir, dataset|
  if dataset == "scratch"
    dir = File.join(data_dir, "scratch-ruby-#{Process.pid}")
    FileUtils.mkdir_p(dir)
    JedEngine.new(Jed.create(File.join(dir, "scratch.jed")), data_dir, dataset, dir)
  else
    JedEngine.new(Jed.open(File.join(data_dir, "#{dataset}.jed")), data_dir, dataset, nil)
  end
end

Bench.main_with(engine: "jed", lang: "ruby", variant: "wrap", open: open_engine)
