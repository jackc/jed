# frozen_string_literal: true

# scripts/bench_results.rb — shared plumbing for the benchmark result reporters
# (bench_report.rb terminal matrix, bench_html.rb static HTML report, bench_markdown.rb
# Markdown report, bench_diff.rb machine-readable diff; spec/design/benchmarks.md §9).
# Loads a run dir's JSONL results, applies the answer/fingerprint verification every
# reporter enforces identically, joins two runs for before/after comparison, and builds
# the renderer-neutral report model the HTML and Markdown reports both consume.

require "json"

module BenchResults
  # Noise floor for delta presentation: single-run wall-clock jitter on a shared
  # machine routinely exceeds a few percent, so |Δ| under this reads as noise, not
  # signal (benchmarks.md §10).
  NOISE_PCT = 5.0

  module_function

  # All run dirs under bench/results/, sorted (the UTC stamp names make that
  # chronological). Newest is last. Restricted to the UTC-stamp naming
  # (YYYYMMDD-HHMMSS) `rake bench:run` writes, so sibling result families that
  # live under bench/results/ but are not bench runs — the rqg firehose's
  # bench/results/rqg/ tree — are not mistaken for the newest run.
  def run_dirs
    Dir.glob("bench/results/*")
       .select { |d| File.directory?(d) && File.basename(d).match?(/\A\d{8}-\d{6}\z/) }
       .sort
  end

  # Resolve the reporters' shared [run_dir, baseline_dir] CLI shape: the run defaults
  # to the newest dir under bench/results/, the baseline to the run immediately before
  # it (nil when there is none, or when suppressed via baseline: false).
  def resolve_dirs(run_dir, baseline_dir, baseline: true)
    runs = run_dirs
    if run_dir.nil?
      abort "no results under bench/results/ — run `rake bench:run`" if runs.empty?
      run_dir = runs.last
    end
    if baseline_dir.nil? && baseline
      idx = runs.index { |d| File.expand_path(d) == File.expand_path(run_dir) }
      baseline_dir = runs[idx - 1] if idx&.positive?
    end
    [run_dir, baseline_dir]
  end

  def load_dir(dir)
    results = Dir.glob(File.join(dir, "*.jsonl")).sort.flat_map do |path|
      File.readlines(path).map { |line| JSON.parse(line) }
    end
    abort "no results in #{dir}" if results.empty?
    results
  end

  # Checksum disagreement within one run = a wrong answer somewhere, treated like a
  # failing conformance test. Returns the failure strings (empty when all agree).
  def checksum_failures(results)
    failures = []
    results.group_by { |r| [r["bench"], r["dataset"]] }.each do |(bench, dataset), group|
      sums = group.map { |r| r["checksum"] }.uniq
      next if sums.size == 1

      failures << "checksum mismatch for #{bench} (#{dataset}):"
      group.group_by { |r| r["checksum"] }.each do |sum, rs|
        who = rs.map { |r| "#{r['engine']}/#{r['lang']}/#{r['variant']}" }.join(", ")
        failures << "  #{sum}: #{who}"
      end
    end
    failures
  end

  # Mixed fingerprints within one run dir = results measured against different data
  # vintages. Returns the failure string, or nil.
  def mixed_fingerprints(results)
    fingerprints = results.map { |r| r["fingerprint"] }.uniq
    return nil if fingerprints.size == 1

    "mixed fingerprints in one run dir (regenerate with `rake bench:setup` and re-run): #{fingerprints.join(', ')}"
  end

  # The run's failures plus the baseline's (prefixed "baseline: "), in one list — what
  # every two-run reporter prints and exits 1 on.
  def verification_failures(results, baseline)
    failures = checksum_failures(results)
    mixed = mixed_fingerprints(results)
    failures << mixed if mixed
    if baseline
      checksum_failures(baseline).each { |f| failures << "baseline: #{f}" }
      mixed = mixed_fingerprints(baseline)
      failures << "baseline: #{mixed}" if mixed
    end
    failures
  end

  def humanize(ns)
    if ns >= 1_000_000_000 then format("%.2fs", ns / 1e9)
    elsif ns >= 1_000_000   then format("%.2fms", ns / 1e6)
    elsif ns >= 1_000       then format("%.1fµs", ns / 1e3)
    else                         "#{ns}ns"
    end
  end

  def delta_class(delta)
    return "noise" if delta.abs < NOISE_PCT

    delta.negative? ? "imp" : "reg"
  end

  # Bench description + SQL for display, from the corpus definitions. benchmarks.toml
  # is ours with a known line shape (single-line double-quoted strings), so a
  # line-based extraction suffices — Ruby has no stdlib TOML parser. Misses just omit
  # the text.
  def bench_meta(path = "bench/corpus/benchmarks.toml")
    meta = {}
    cur = nil
    return meta unless File.file?(path)

    File.readlines(path).each do |line|
      stripped = line.strip
      if stripped == "[[bench]]"
        cur = {}
      elsif cur && (m = stripped.match(/\A(name|description|sql)\s*=\s*"(.*)"\z/))
        cur[m[1]] = m[2]
        meta[m[2]] = cur if m[1] == "name"
      end
    end
    meta
  end

  # The renderer-neutral report model bench_html.rb and bench_markdown.rb share: one
  # section per (bench, dataset) in first-seen order; rows sorted fastest-first with
  # humanized values, bar widths linear vs the slowest in the group, multipliers vs
  # the fastest, and per-pair deltas from the baseline join. Also lists, per section,
  # engines present only in the baseline, and whole benches present only in the
  # baseline (a filtered run diffed against a full one).
  def report_model(results, joined)
    diff_by_key = joined.to_h { |row| [row[:key], row] }
    baseline_only_by_group = joined.select { |row| row[:after].nil? }
                                   .group_by { |row| row[:key][0, 2] }
    groups = results.group_by { |r| [r["bench"], r["dataset"]] }
    meta = bench_meta
    sections = groups.map do |(bench, dataset), group|
      sorted = group.sort_by { |r| r["ns_per_op"] }
      fastest = sorted.first["ns_per_op"]
      slowest = sorted.last["ns_per_op"]
      rows = sorted.map do |r|
        diff = diff_by_key[[r["bench"], r["dataset"], r["engine"], r["lang"], r["variant"]]]
        {
          label: "#{r['engine']}/#{r['lang']}/#{r['variant']}",
          family: r["engine"],
          width_pct: [r["ns_per_op"] * 100.0 / slowest, 0.4].max,
          ns: humanize(r["ns_per_op"]),
          mult: format("%.1f×", r["ns_per_op"].to_f / fastest),
          tooltip: "min #{humanize(r['min_ns'])} · p50 #{humanize(r['p50_ns'])} · " \
                   "#{r['iterations']} iterations · #{r['rows_total']} rows",
          delta: diff&.dig(:delta_pct),
          delta_tooltip: diff && diff[:before] &&
            "baseline #{humanize(diff[:before]['ns_per_op'])} → #{humanize(r['ns_per_op'])}",
        }
      end
      missing = (baseline_only_by_group[[bench, dataset]] || []).map { |row| row[:key][2, 3].join("/") }
      info = meta[bench] || {}
      first = sorted.first
      {
        bench: bench, dataset: dataset, rows: rows, missing: missing,
        description: info["description"], sql: info["sql"],
        subtitle: "checksum #{first['checksum']} · #{first['iterations']} iterations · #{first['rows_total']} rows",
      }
    end
    { sections: sections,
      baseline_only_benches: baseline_only_by_group.keys.reject { |k| groups.key?(k) } }
  end

  # Join two runs on (bench, dataset, engine, lang, variant) for before/after
  # comparison. Order: the run's first-seen order, then baseline-only rows in the
  # baseline's first-seen order. Each row carries the result hashes (nil for a missing
  # side), delta_pct ((after - before) / before * 100, nil unless both sides present),
  # and checksum_match (nil unless both sides present). Partial/filtered runs are
  # explicit — an unmatched pair is a row, never silently dropped.
  def join_runs(run_results, baseline_results)
    key = ->(r) { [r["bench"], r["dataset"], r["engine"], r["lang"], r["variant"]] }
    before_by_key = baseline_results.to_h { |r| [key.call(r), r] }
    rows = run_results.map do |after|
      before = before_by_key.delete(key.call(after))
      delta = before && (after["ns_per_op"] - before["ns_per_op"]) * 100.0 / before["ns_per_op"]
      { key: key.call(after), before: before, after: after, delta_pct: delta,
        checksum_match: before && before["checksum"] == after["checksum"] }
    end
    rows + before_by_key.values.map do |before|
      { key: key.call(before), before: before, after: nil, delta_pct: nil, checksum_match: nil }
    end
  end
end
