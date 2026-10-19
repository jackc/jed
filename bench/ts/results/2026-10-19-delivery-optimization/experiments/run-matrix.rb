# frozen_string_literal: true

# Run every host/variant serially through the existing mise benchmark tasks. The per-process
# harness alternates direct/worker samples; this outer matrix reverses variant order on repeat 2.
require "digest"
require "fileutils"
require "json"

MODES = %w[pilot repeated coverage cancel].freeze
DEFAULT_VARIANTS = %w[baseline hex codec dispatch combined].freeze
PRIMARY_CASES = %w[decimal-aggregate typed-output].freeze
COVERAGE_CASES = %w[resident-join-group sort-top100].freeze

def verify_source(source, manifest, variant)
  abort "Variant label mismatch: #{source}" unless manifest.fetch("label") == variant
  hashes = manifest.fetch("sourceSha256")
  abort "Empty source manifest: #{source}" if hashes.empty?
  hashes.each do |file, expected|
    actual = Digest::SHA256.file(File.join(source, file)).hexdigest
    abort "Source changed: #{source}/#{file}" unless actual == expected
  end
  patch_hash = Digest::SHA256.file(File.join(source, "variant.patch")).hexdigest
  abort "Variant patch changed: #{source}" unless patch_hash == manifest.fetch("patchSha256")
end

def report_result(file, mode, host, variant, expected_cases)
  data = JSON.parse(File.read(file))
  suite = data.fetch("suite", data)
  # Cancellation uses results, not the latency harness's cases schema.
  records = suite.fetch(mode == "cancel" ? "results" : "cases")
  unless records.map { |record| record.fetch("id") } == expected_cases
    abort "Unexpected cases: #{file}"
  end
  records.each do |record|
    if mode == "cancel"
      samples = record.fetch("samples")
      abort "Missing cancellation samples: #{file}" if samples.empty?
      unless samples.all? { |sample| sample.fetch("cleanupVerified") && !sample.fetch("watchdogFired") }
        abort "Cancellation cleanup or watchdog failure: #{file}"
      end
      canceled = samples.count { |sample| sample.fetch("state") == "canceled" }
      abort "Cancellation count mismatch: #{file}" unless canceled == record.fetch("canceled")
      median = record.dig("issueToSettleMs", "median")
      puts "#{host} #{variant} #{record.fetch('id')}: canceled=#{canceled}/#{samples.length} issue-to-settle median=#{median&.round(2)} ms"
    else
      direct = record.fetch("latencyMs").fetch("direct").fetch("p50")
      worker = record.fetch("latencyMs").fetch("worker").fetch("p50")
      unless [direct, worker].all? { |value| value.is_a?(Numeric) && value.finite? && value >= 0 }
        abort "Invalid latency: #{file}"
      end
      puts "#{host} #{variant} #{record.fetch('id')}: direct=#{direct.round(2)} worker=#{worker.round(2)}"
    end
  end
end

root = File.expand_path(ENV.fetch("JED_EXPERIMENT_REPO", "../../../../.."), __dir__)
mode = ARGV.shift || "pilot"
abort "Mode must be #{MODES.join(', ')}" unless MODES.include?(mode)
variants = ARGV.empty? ? DEFAULT_VARIANTS : ARGV
unless variants.uniq == variants && variants.all? { |variant| /\A[a-z][a-z0-9_]*\z/.match?(variant) }
  abort "Variant names must be unique lowercase labels without path separators"
end
repetitions = %w[pilot coverage].include?(mode) ? 1 : 3
base = File.expand_path(ENV.fetch("JED_EXPERIMENT_RESULTS", File.join(root, "bench/results/delivery-optimization-20261019")))
cases = mode == "coverage" ? COVERAGE_CASES : PRIMARY_CASES

repetitions.times do |repeat|
  order = repeat.odd? ? variants.reverse : variants
  %w[node browser].each do |host|
    order.each do |variant|
      source = File.join(base, "sources", variant)
      manifest = JSON.parse(File.read(File.join(source, "variant.json")))
      verify_source(source, manifest, variant)
      out = File.join(base, mode, "#{host}-#{variant}-#{repeat + 1}")
      abort "Output exists: #{out}" if File.exist?(out)
      FileUtils.mkdir_p(out)

      env = {
        "JED_PROBE_ROOT" => source,
        "JED_ASYNC_HEAVY_CASES" => cases.join(","),
        "JED_ASYNC_HEAVY_WARMUPS" => mode == "pilot" ? "2" : "3",
        "JED_ASYNC_HEAVY_SAMPLES" => mode == "pilot" ? "3" : "7",
        "JED_ASYNC_HEAVY_RESPONSE_SAMPLES" => mode == "pilot" ? "1" : "3"
      }
      if mode == "cancel"
        env.merge!("JED_ASYNC_CANCEL_HOST" => host, "JED_ASYNC_CANCEL_RESULTS" => out)
        task = "bench:ts_async_cancel"
      else
        env[host == "node" ? "JED_ASYNC_RESULTS" : "JED_ASYNC_BROWSER_RESULTS"] = out
        task = host == "node" ? "bench:ts_async_heavy" : "bench:ts_async_browser"
      end

      puts "START #{mode} #{host} #{variant} #{repeat + 1}"
      STDOUT.flush
      ok = system(
        env, "taskset", "-c", "0,2", "mise", "run", task,
        chdir: root, out: File.join(out, "run.log"), err: [:child, :out]
      )
      abort "Failed #{out}; see run.log" unless ok
      verify_source(source, manifest, variant)
      FileUtils.cp(File.join(source, "variant.json"), File.join(out, "variant.json"))
      report_result(File.join(out, "results.json"), mode, host, variant, cases)
      STDOUT.flush
    end
  end
end
