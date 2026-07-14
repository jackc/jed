# frozen_string_literal: true

# Shared plumbing for the Ruby benchmark harness (spec/design/benchmarks.md). A faithful port of
# bench/rust/src/lib.rs: the splitmix64 param stream, the FNV-1a answer checksum, corpus/dataset
# parsing, the fingerprint gate, and the engine-agnostic run loop. bench_jed.rb supplies the driver.
#
# This module measures the **Ruby gem's overhead** by running the shared corpus through the gem
# (engine=jed, lang=ruby, variant=wrap); the report lines `jed/ruby/wrap` up against `jed/rust/core`,
# and the per-bench delta is the binding tax. The cross-engine answer checksum doubles as a
# correctness gate — the gem must return byte-identical rows to the core.

require "json"
require "digest"
require "toml-rb"

module Bench
  MASK64 = 0xFFFF_FFFF_FFFF_FFFF

  # splitmix64 (benchmarks.md §4). Ruby integers are arbitrary-precision, so every wrapping op is
  # masked back to 64 bits to match the Rust/Go/TS ports bit-for-bit.
  class Prng
    def initialize(seed)
      @z = seed & MASK64
    end

    def next_u64
      @z = (@z + 0x9E37_79B9_7F4A_7C15) & MASK64
      x = @z
      x = ((x ^ (x >> 30)) * 0xBF58_476D_1CE4_E5B9) & MASK64
      x = ((x ^ (x >> 27)) * 0x94D0_49BB_1331_11EB) & MASK64
      (x ^ (x >> 31)) & MASK64
    end

    # Bounded draw in [lo, hi] — modulo bias accepted, identical everywhere.
    def int_uniform(lo, hi)
      lo + (next_u64 % (hi - lo + 1))
    end

    # Lowercase ASCII string, length in [min_len, max_len].
    def text(min_len, max_len)
      n = int_uniform(min_len, max_len)
      s = +""
      n.times { s << (97 + (next_u64 % 26)).chr }
      s
    end
  end

  # FNV-1a 64 answer checksum (benchmarks.md §6).
  class Checksum
    FNV_OFFSET = 0xcbf2_9ce4_8422_2325
    FNV_PRIME  = 0x0000_0100_0000_01b3

    def initialize
      @h = FNV_OFFSET
    end

    def add_bytes(str)
      str.each_byte { |b| @h = ((@h ^ b) * FNV_PRIME) & MASK64 }
    end

    def sep(byte)
      @h = ((@h ^ byte) * FNV_PRIME) & MASK64
    end

    def null
      add_bytes("NULL")
      sep(0x1F)
    end

    def int(n)
      add_bytes(n.to_s)
      sep(0x1F)
    end

    def text(s)
      add_bytes(s)
      sep(0x1F)
    end

    def end_row
      sep(0x1E)
    end

    def hex
      format("%016x", @h)
    end
  end

  # int_window: base is the 0-based index of an EARLIER param; the value is that param's value +
  # int_uniform(off_min, off_max) — a selective fixed-width range around a base param.
  Param = Struct.new(:generator, :min, :max, :start, :min_len, :max_len, :base, :off_min, :off_max)

  Workload = Struct.new(
    :name, :dataset, :kind, :sql, :warmup, :iterations, :seed, :expect_rows_per_iter,
    :engines, :batch, :setup_sql, :sql_override, :setup_sql_override, :params
  ) do
    def sql_for(engine) = sql_override[engine] || sql
    def setup_sql_for(engine) = setup_sql_override[engine] || setup_sql
    def runs_on?(engine) = engines.empty? || engines.include?(engine)
  end

  module_function

  def load_corpus(corpus_dir)
    root = TomlRB.load_file(File.join(corpus_dir, "benchmarks.toml"))
    raise "benchmarks.toml: unsupported schema_version" unless root["schema_version"] == 1

    (root["bench"] || []).map do |t|
      params = (t["param"] || []).map do |p|
        Param.new(p["gen"] || "", p["min"] || 0, p["max"] || 0, p["start"] || 0,
          p["min_len"] || 0, p["max_len"] || 0, p["base"] || 0, p["off_min"] || 0, p["off_max"] || 0)
      end
      Workload.new(
        t["name"] || "", t["dataset"] || "", t["kind"] || "", t["sql"] || "",
        t["warmup"] || 0, t["iterations"] || 0, t["seed"] || 0, t["expect_rows_per_iter"],
        t["engines"] || [], t["batch"] || 0, t["setup_sql"] || [],
        t["sql_override"] || {}, t["setup_sql_override"] || {}, params
      )
    end
  end

  def dataset_table_rows(corpus_dir, dataset, table)
    root = TomlRB.load_file(File.join(corpus_dir, "datasets.toml"))
    ds = (root["dataset"] || []).find { |d| d["name"] == dataset } or
      raise "datasets.toml: no dataset #{dataset}"
    tb = (ds["table"] || []).find { |t| t["name"] == table } or
      raise "datasets.toml: no table #{table} in dataset #{dataset}"
    tb["rows"]
  end

  # benchmarks.md §5: the fingerprint is SHA-256 of datasets.toml; the sidecar pins the engine's
  # generated data to it. The gem reuses the "jed" datasets + sidecars (same byte format).
  def corpus_fingerprint(corpus_dir)
    Digest::SHA256.hexdigest(File.binread(File.join(corpus_dir, "datasets.toml")))
  end

  def read_sidecar(data_dir, dataset, engine)
    path = File.join(data_dir, "#{dataset}.#{engine}.fingerprint")
    File.exist?(path) ? File.read(path).strip : ""
  end

  # The target table of a write statement — the word after INTO / FROM — for the post-run count.
  def insert_table(sql)
    fields = sql.split
    fields.each_with_index do |f, i|
      if (f.casecmp?("INTO") || f.casecmp?("FROM")) && i + 1 < fields.length
        return fields[i + 1].split("(").first
      end
    end
    raise "write bench SQL has no INSERT INTO / DELETE FROM table: #{sql}"
  end

  # One stream of args across warmup + measured iterations (benchmarks.md §3) — a serial counter or
  # a PRNG draw per param. Returns plain Integers/Strings (the gem binds them directly).
  class ParamStream
    def initialize(workload)
      @params = workload.params
      @prng = Prng.new(workload.seed)
      @serials = @params.map(&:start)
    end

    def next_args
      # Built incrementally (not .map) so int_window can reference an EARLIER arg in the same row.
      args = []
      @params.each_with_index do |p, i|
        args << case p.generator
        when "serial" then (v = @serials[i]; @serials[i] += 1; v)
        when "int_uniform" then @prng.int_uniform(p.min, p.max)
        when "int_window" then args[p.base] + @prng.int_uniform(p.off_min, p.off_max)
        when "text" then @prng.text(p.min_len, p.max_len)
        else raise "unknown param gen #{p.generator}"
        end
      end
      args
    end
  end

  ResultLine = Struct.new(
    :bench, :dataset, :iterations, :warmup, :total_ns, :ns_per_op, :min_ns, :p50_ns,
    :p90_ns, :p99_ns,
    :rows_total, :checksum, :fingerprint, :started_at
  ) do
    # The JSONL contract (benchmarks.md §6) — field order matches the other harnesses exactly.
    def to_json_line(cfg)
      %({"schema":2,"bench":"#{bench}","dataset":"#{dataset}","engine":"#{cfg[:engine]}",) +
        %("lang":"#{cfg[:lang]}","variant":"#{cfg[:variant]}","iterations":#{iterations},) +
        %("warmup":#{warmup},"readers":0,"total_ns":#{total_ns},"ns_per_op":#{ns_per_op},"min_ns":#{min_ns},) +
        %("p50_ns":#{p50_ns},"p90_ns":#{p90_ns},"p99_ns":#{p99_ns},) +
        %("rows_total":#{rows_total},"checksum":"#{checksum}",) +
        %("fingerprint":"#{fingerprint}","started_at":"#{started_at}"})
    end
  end

  def now_ns = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)

  def run(cfg, corpus_dir, data_dir, filter)
    want = corpus_fingerprint(corpus_dir)
    load_corpus(corpus_dir).filter_map do |w|
      next if !filter.empty? && !w.name.include?(filter)
      next unless w.runs_on?(cfg[:engine])
      # concurrent_read needs the host session API + true parallelism; the gem is autocommit
      # with no Session handle and Ruby's GIL precludes parallel readers, so it opts out
      # (spec/design/benchmarks.md §8.1). The native cores carry that bench.
      if w.kind == "concurrent_read"
        warn "  skip: #{cfg[:engine]}/#{cfg[:lang]}/#{cfg[:variant]} has no concurrent_read support"
        next
      end

      warn "#{cfg[:engine]}/#{cfg[:lang]}/#{cfg[:variant]}: #{w.name} (#{w.dataset}) ..."
      run_one(cfg, w, corpus_dir, data_dir, want).to_json_line(cfg)
    end
  end

  def run_one(cfg, w, corpus_dir, data_dir, want)
    eng = cfg[:open].call(data_dir, w.dataset)
    if w.dataset != "scratch" && eng.stored_fingerprint != want
      raise "stale benchmark data for #{w.dataset}/#{cfg[:engine]}: run 'rake bench:setup'"
    end

    w.setup_sql_for(cfg[:engine]).each { |sql| eng.exec(sql) }
    eng.prepare(w.sql_for(cfg[:engine]))

    started_at = Time.now.utc.strftime("%Y-%m-%dT%H:%M:%SZ")
    stream = ParamStream.new(w)
    sum = Checksum.new
    elapsed = []
    rows_total = 0
    alloc0 = GC.stat(:total_allocated_objects)

    (w.warmup + w.iterations).times do |i|
      measured = i >= w.warmup
      case w.kind
      when "query"
        args = stream.next_args
        t0 = now_ns
        n = eng.query_prepared(args, measured ? sum : nil)
        d = now_ns - t0
        if measured
          elapsed << d
          rows_total += n
          if w.expect_rows_per_iter && n != w.expect_rows_per_iter
            raise "expected #{w.expect_rows_per_iter} rows per iteration, got #{n}"
          end
        end
      when "write_rollback"
        t0 = now_ns
        eng.exec("BEGIN")
        w.batch.times { eng.exec_prepared(stream.next_args) }
        eng.exec("ROLLBACK")
        elapsed << (now_ns - t0) if measured
      when "write_durable"
        args = stream.next_args
        t0 = now_ns
        eng.exec_prepared(args)
        elapsed << (now_ns - t0) if measured
      else
        raise "unknown bench kind #{w.kind}"
      end
    end

    # Allocations/op — a DETERMINISTIC overhead metric (unlike wall-clock); printed to stderr only,
    # never the JSONL (keeps the cross-engine contract identical).
    allocs_per_op = (GC.stat(:total_allocated_objects) - alloc0) / (w.warmup + w.iterations)

    if w.kind != "query"
      table = insert_table(w.sql)
      n = eng.query_int("SELECT count(*) FROM #{table}")
      expect = w.kind == "write_rollback" ? dataset_table_rows(corpus_dir, w.dataset, table) : (w.warmup + w.iterations)
      raise "post-run count(*) of #{table}: got #{n}, want #{expect}" if n != expect

      sum.int(n)
      sum.end_row
    end

    eng.close if eng.respond_to?(:close)

    elapsed.sort!
    total_ns = elapsed.sum
    ns_per_op = total_ns / w.iterations
    p50 = percentile(elapsed, 50)
    p90 = percentile(elapsed, 90)
    p99 = percentile(elapsed, 99)
    warn "  → #{ns_per_op} ns/op, #{allocs_per_op} allocs/op (p50/p90/p99 #{p50}/#{p90}/#{p99})"
    ResultLine.new(
      w.name, w.dataset, w.iterations, w.warmup, total_ns, ns_per_op,
      elapsed[0], p50, p90, p99, rows_total, sum.hex, want, started_at
    )
  end

  # Lower sample percentile from an already-sorted, non-empty distribution. This
  # preserves the historical lower-median definition for p50.
  def percentile(sorted, pct) = sorted[(sorted.length - 1) * pct / 100]

  # Uniform entrypoint: bench_jed.rb <corpus_dir> <data_dir> <out_path> [name_filter].
  def main_with(cfg)
    if ARGV.length < 3 || ARGV.length > 4
      warn "usage: #{$PROGRAM_NAME} <corpus_dir> <data_dir> <out_path> [name_filter]"
      exit 2
    end
    corpus_dir, data_dir, out_path, filter = ARGV[0], ARGV[1], ARGV[2], (ARGV[3] || "")
    lines = run(cfg, corpus_dir, data_dir, filter)
    File.write(out_path, lines.empty? ? "" : "#{lines.join("\n")}\n")
  rescue => e
    warn "error: #{e.message}"
    exit 1
  end
end
