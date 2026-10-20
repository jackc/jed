#!/usr/bin/env ruby
# frozen_string_literal: true

# Summarize bench/aead/codec results.json into Markdown tables (README.md in this directory).
# Percentiles are nearest-rank over the pooled samples of every process run; "run medians" is the
# range of each run's own median, which shows process-to-process variation.
require "json"

input, output = ARGV
abort "usage: summarize.rb results.json summary.md" unless input && output
data = JSON.parse(File.read(input))
runs = data.fetch("runs")
variants = runs.first.fetch("variantOrder").sort_by { |v| runs.first["variantOrder"].index(v) }
order = %w[none identity-async aes-gcm-w16 aes-gcm-w4 aes-gcm-w1 aes-gcm-yield xchacha-sync]
variants = order.select { |v| variants.include?(v) } + (variants - order)

def pct(values, fraction)
  sorted = values.sort
  return nil if sorted.empty?
  sorted[[(sorted.length * fraction).ceil - 1, 0].max]
end

def median(values) = pct(values, 0.5)
def fmt(x, digits = 2) = x.nil? ? "—" : format("%.#{digits}f", x)

def by_variant(runs)
  runs.each_with_object(Hash.new { |h, k| h[k] = [] }) do |run, acc|
    run["results"].each { |r| acc[r["variant"]] << r }
  end
end

results = by_variant(runs)
lines = []
meta = data["metadata"]
lines << "# Async page codec over real OPFS — summary"
lines << ""
lines << "Generated from `results.json` (#{meta['dateUtc']}, revision `#{meta['revision']}`" \
         "#{meta['dirty'] ? ', dirty tree' : ''}). #{runs.length} process runs, each a fresh headless " \
         "Chromium #{meta['browser']} (V8 #{meta['jsVersion']}); #{meta['cpu']}, #{meta['logicalCpus']} " \
         "logical CPUs, #{meta['platform']} #{meta['release']}. Cross-origin isolated: " \
         "#{runs.first.dig('environment', 'crossOriginIsolated')}."
lines << ""
lines << "Config: `#{JSON.generate(runs.first['config'])}`"
lines << ""

# Integrity: identical files and query checks across variants within each run, and across runs.
hashes = runs.flat_map { |run| run["results"].map { |r| r.dig("finish", "fileSha256") } }.uniq
checks = runs.flat_map { |run| run["results"].map { |r| JSON.generate(r.dig("finish", "checks")) } }.uniq
lines << "## Integrity"
lines << ""
lines << "- Final database file SHA-256 identical across all variants and runs: " \
         "**#{hashes.length == 1 ? 'yes' : "NO (#{hashes.length} distinct)"}** (`#{hashes.first}`, " \
         "#{runs.first['results'].first.dig('finish', 'fileBytes')} bytes)."
lines << "- Query checks identical: **#{checks.length == 1 ? 'yes' : 'NO'}** (`#{checks.first}`)."
counted = variants.reject { |v| v == "none" }.map do |v|
  enc = results[v].sum { |r| r.dig("finish", "encodedTotal") }
  dec = results[v].sum { |r| r.dig("finish", "decodedTotal") }
  "#{v} #{enc}/#{dec}"
end
lines << "- Pages sealed/opened by each codec over all runs: #{counted.join(', ')}."
lines << ""

probe = runs.flat_map { |run| run["webCryptoProbe"] || [] }
unless probe.empty?
  per_op = probe.map { |o| o["workerMs"] * 1000.0 / o.dig("extra", "operations") }
  lines << "## WebCrypto scheduling probe (no engine)"
  lines << ""
  lines << "A chain of #{probe.first.dig('extra', 'operations')} awaited #{runs.first.dig('results', 0, 'setup', 'pageSize')}-byte " \
           "AES-GCM seals in the worker, #{probe.length} observations: median #{fmt(median(per_op), 2)} µs/seal; " \
           "time inside the `encrypt()` call median #{fmt(median(probe.map { |o| o.dig('extra', 'inCallFraction') * 100 }), 1)}%; " \
           "pings admitted while the chain ran: #{probe.sum { |o| o['pingsAdmittedDuringOp'] }} of " \
           "#{probe.sum { |o| o['pingsSent'] }} sent; max RTT median " \
           "#{fmt(median(probe.map { |o| o['pingRttMs'].max || 0 }), 2)} ms vs chain median " \
           "#{fmt(median(probe.map { |o| o['workerMs'] }), 2)} ms."
  lines << ""
end

METRICS = [
  ["single", "Durable single-row autocommit INSERT (statement total)"],
  ["tx64Commit", "64-row transaction: COMMIT"],
  ["tx64Total", "64-row transaction: BEGIN + INSERT … SELECT + COMMIT"],
  ["largeCommit", "Large transaction COMMIT"],
  ["coldOpen", "Cold open (last commit large: manifest validation)"],
  ["coldScan", "Cold full scan (first scan after open)"],
  ["warmScan", "Warm full scan (page cache resident)"],
].freeze

lines << "## Latency (ms)"
lines << ""
lines << "Pooled median and p95 over all runs; the run-median range shows per-process variation. " \
         "Pages: median pages sealed (enc) / opened (dec) by the codec per operation. " \
         "Δ/page: (variant median − none median) / pages crossing the codec, in µs. " \
         "Crypto: time from issuing each crypto call to holding its result, summed per operation " \
         "(identity-async has none); its per-page median is per sample."
lines << ""
none_medians = {}
METRICS.each do |key, title|
  lines << "### #{title}"
  lines << ""
  lines << "| Variant | n | median | p95 | run medians | vs none | pages enc/dec | Δ/page (µs) | crypto ms (median) | crypto µs/page |"
  lines << "|---|---:|---:|---:|---|---:|---:|---:|---:|---:|"
  variants.each do |v|
    samples = results[v].flat_map { |r| r.dig("latency", key) }
    ms = samples.map { |s| s["ms"] }
    run_medians = results[v].map { |r| median(r.dig("latency", key).map { |s| s["ms"] }) }
    m = median(ms)
    none_medians[key] = m if v == "none"
    base = none_medians[key]
    enc = median(samples.map { |s| s["encoded"] })
    dec = median(samples.map { |s| s["decoded"] })
    pages = enc + dec
    delta = v != "none" && base && pages.positive? ? (m - base) * 1000.0 / pages : nil
    ratio = base ? m / base : nil
    crypto = samples.map { |s| s["cryptoMs"] || 0 }
    crypto_page = samples.select { |s| (s["encoded"] + s["decoded"]).positive? }
                         .map { |s| (s["cryptoMs"] || 0) * 1000.0 / (s["encoded"] + s["decoded"]) }
    show = v != "none" && pages.positive?
    lines << "| #{v} | #{ms.length} | #{fmt(m, 3)} | #{fmt(pct(ms, 0.95), 3)} | " \
             "#{fmt(run_medians.min, 3)}–#{fmt(run_medians.max, 3)} | #{fmt(ratio)}× | " \
             "#{enc}/#{dec} | #{fmt(delta, 1)} | #{show ? fmt(median(crypto), 3) : '—'} | " \
             "#{show ? fmt(median(crypto_page), 1) : '—'} |"
  end
  lines << ""
end

lines << "## Responsiveness"
lines << ""
idle = runs.flat_map { |run| run.dig("idle", "pingRttMs") }
lines << "The main thread posts a ping to the engine worker every #{runs.first['pingIntervalMs']} ms " \
         "(Chromium clamps to ~4 ms) while one operation runs; the worker echoes it from a port " \
         "handler, which runs only when the worker's event loop is free. Idle-worker floor: " \
         "p50 #{fmt(median(idle), 3)} / p95 #{fmt(pct(idle, 0.95), 3)} / max #{fmt(idle.max, 3)} ms."
lines << "Max RTT per observation approximates the longest stretch the worker could not admit a " \
         "control message (cancel/close). Frame gap: the main thread's largest animation-frame gap."
lines << ""
names = runs.first["results"].first["observations"].map { |o| o["name"] }.uniq
lines << "| Operation | Variant | obs | op median (ms) | pings admitted / sent | max RTT median (ms) | max RTT worst (ms) | all-ping p95 (ms) | main frame gap worst (ms) |"
lines << "|---|---|---:|---:|---:|---:|---:|---:|---:|"
names.each do |name|
  variants.each do |v|
    obs = results[v].flat_map { |r| r["observations"].select { |o| o["name"] == name } }
    maxes = obs.map { |o| o["pingRttMs"].max || 0 }
    all = obs.flat_map { |o| o["pingRttMs"] }
    frames = obs.flat_map { |o| o["frameGapsMs"] }
    lost = obs.sum { |o| o["pingsLost"] }
    admitted = obs.sum { |o| o["pingsAdmittedDuringOp"] }
    lines << "| #{name} | #{v} | #{obs.length} | #{fmt(median(obs.map { |o| o['workerMs'] }), 2)} | " \
             "#{admitted} / #{obs.sum { |o| o['pingsSent'] }} | " \
             "#{fmt(median(maxes), 2)} | #{fmt(maxes.max, 2)} | #{fmt(pct(all, 0.95), 2)} | " \
             "#{fmt(frames.max, 1)}#{lost.positive? ? " (#{lost} pings lost)" : ''} |"
  end
end
lines << ""
File.write(output, lines.join("\n") + "\n")
puts File.read(output) if ENV["JED_AEAD_CODEC_PRINT"] == "1"
