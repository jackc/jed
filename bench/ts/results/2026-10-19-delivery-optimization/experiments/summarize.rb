# frozen_string_literal: true

# Timing/response and cancellation summaries retain their separate sample schemas.
require "digest"
require "json"

def numeric_samples(values, context)
  unless values.is_a?(Array) && !values.empty? &&
         values.all? { |value| value.is_a?(Numeric) && value.finite? && value >= 0 }
    abort "Expected nonempty finite nonnegative samples: #{context}"
  end
  values
end

def median(values)
  sorted = values.sort
  middle = sorted.length / 2
  sorted.length.odd? ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2.0
end

abort "Usage: ruby summarize.rb RESULTS_ROOT OUTPUT.json" unless ARGV.length == 2
root, output = ARGV.map { |path| File.expand_path(path) }
files = Dir[File.join(root, "{pilot,repeated,coverage}/*/results.json")].sort
cancel_files = Dir[File.join(root, "cancel/*/results.json")].sort
abort "No results found: #{root}" if files.empty? && cancel_files.empty?
validated_results = {}
runs = files.map do |file|
  data = JSON.parse(File.read(file))
  suite = data.fetch("suite", data)
  phase, name = file.delete_prefix(root + "/").split("/")
  match = /\A(node|browser)-([a-z][a-z0-9_]*)-([1-9][0-9]*)\z/.match(name)
  abort "Unexpected run directory: #{file}" unless match
  host, variant, repeat = match.captures
  config = suite.fetch("config")
  cases = suite.fetch("cases")
  unless cases.map { |record| record.fetch("id") } == config.fetch("cases")
    abort "Configured case list differs from results: #{file}"
  end
  {
    phase: phase, host: host, variant: variant, repeat: repeat.to_i,
    result: file.delete_prefix(root + "/"), sha256: Digest::SHA256.file(file).hexdigest,
    config: config,
    cases: cases.map do |record|
      id = record.fetch("id")
      validated = record.fetch("validated")
      if validated_results.key?(id) && validated_results[id] != validated
        abort "Result values, metadata or SQL cost differ for #{id}: #{file}"
      end
      validated_results[id] = validated
      %w[direct worker].each do |boundary|
        samples = numeric_samples(record.fetch("rawSamplesMs").fetch(boundary), "#{file}: #{id}/#{boundary}")
        abort "Incorrect latency sample count: #{file}" unless samples.length == config.fetch("samples")
        p50 = record.fetch("latencyMs").fetch(boundary).fetch("p50")
        abort "Incorrect latency median: #{file}" unless p50 == median(samples)
      end
      response = record.fetch("responsiveness").transform_values do |observations|
        unless observations.length == config.fetch("responseSamples")
          abort "Incorrect responsiveness sample count: #{file}"
        end
        timers = numeric_samples(
          observations.map { |observation| observation.fetch("timer").fetch("gaps").fetch("max") },
          "#{file}: #{id} timer maxima"
        )
        frames = observations.filter_map { |observation| observation.dig("extra", "animationFrames", "summary", "maxMs") }
        tasks = observations.filter_map { |observation| observation.dig("extra", "longTasks", "count") }
        numeric_samples(frames, "#{file}: #{id} frame maxima") unless frames.empty?
        unless tasks.all? { |count| count.is_a?(Integer) && count >= 0 }
          abort "Invalid long-task counts: #{file}"
        end
        { timerMaxMs: timers, frameMaxMs: frames, longTasks: tasks }
      end
      {
        id: id, validated: validated,
        latency: record.fetch("latencyMs"), samples: record.fetch("rawSamplesMs"),
        responsiveness: response
      }
    end
  }
end

groups = runs.group_by { |run| [run[:phase], run[:host], run[:variant]] }.map do |(phase, host, variant), group|
  abort "Configuration changed across #{phase}/#{host}/#{variant}" unless group.map { |run| run[:config] }.uniq.length == 1
  cases = group.flat_map { |run| run[:cases] }.group_by { |record| record[:id] }.map do |id, records|
    latency = %w[direct worker].to_h do |boundary|
      process_medians = records.map { |record| record[:latency].fetch(boundary).fetch("p50") }
      samples = records.flat_map { |record| record[:samples].fetch(boundary) }
      [
        boundary,
        {
          processMediansMs: process_medians, medianOfMediansMs: median(process_medians),
          pooledMeanMs: samples.sum / samples.length, minMs: samples.min, maxMs: samples.max
        }
      ]
    end
    response = %w[direct worker].to_h do |boundary|
      observations = records.map { |record| record[:responsiveness].fetch(boundary) }
      timers = observations.flat_map { |observation| observation[:timerMaxMs] }
      frames = observations.flat_map { |observation| observation[:frameMaxMs] }
      tasks = observations.flat_map { |observation| observation[:longTasks] }
      [
        boundary,
        {
          observations: timers.length, medianTimerMaxMs: median(timers), worstTimerMs: timers.max,
          medianFrameMaxMs: frames.empty? ? nil : median(frames), worstFrameMs: frames.max,
          longTasks: tasks.empty? ? nil : tasks.sum
        }
      ]
    end
    { id: id, latency: latency, responsiveness: response }
  end
  { phase: phase, host: host, variant: variant, runs: group.length, cases: cases }
end

cancellation_runs = cancel_files.map do |file|
  data = JSON.parse(File.read(file))
  suite = data.fetch("suite", data)
  name = File.basename(File.dirname(file))
  match = /\A(node|browser)-([a-z][a-z0-9_]*)-([1-9][0-9]*)\z/.match(name)
  abort "Unexpected cancellation directory: #{file}" unless match
  host, variant, repeat = match.captures
  config = suite.fetch("config")
  limits = suite.fetch("limits")
  expected_limits = { "maxBatchRows" => 512, "maxBatchBytes" => 262_144, "maxResultBytes" => 2_097_152 }
  abort "Cancellation limits changed: #{file}" unless limits == expected_limits
  cases = suite.fetch("results").map do |record|
    id = record.fetch("id")
    validated = record.fetch("validated")
    if validated_results.key?(id) && validated_results[id] != validated
      abort "Cancellation validation differs for #{id}: #{file}"
    end
    validated_results[id] = validated
    samples = record.fetch("samples")
    unless samples.length == config.fetch("samples") && record.fetch("canceled") == samples.length
      abort "Incorrect canceled sample count: #{file}"
    end
    samples.each do |sample|
      unless sample.fetch("state") == "canceled" && sample.fetch("code") == "57014" &&
             sample.fetch("cleanupVerified") && !sample.fetch("watchdogFired")
        abort "Cancellation, cleanup or watchdog failure: #{file}"
      end
      elapsed = sample.fetch("settledAtMs") - sample.fetch("cancelIssuedAtMs")
      unless (elapsed - sample.fetch("issueToSettleMs")).abs < 0.000001
        abort "Cancellation clock arithmetic mismatch: #{file}"
      end
    end
    elapsed = numeric_samples(samples.map { |sample| sample.fetch("issueToSettleMs") }, "#{file}: #{id}")
    reported = record.fetch("issueToSettleMs")
    unless reported.fetch("samples") == elapsed.length && reported.fetch("median") == median(elapsed) &&
           reported.fetch("min") == elapsed.min && reported.fetch("max") == elapsed.max
      abort "Incorrect cancellation summary: #{file}"
    end
    { id: id, validated: validated, processMedianMs: median(elapsed), samplesMs: elapsed }
  end
  {
    host: host, variant: variant, repeat: repeat.to_i,
    result: file.delete_prefix(root + "/"), sha256: Digest::SHA256.file(file).hexdigest,
    config: config, limits: limits, cases: cases
  }
end
cancellation_groups = cancellation_runs.group_by { |run| [run[:host], run[:variant]] }.map do |(host, variant), group|
  abort "Cancellation configuration changed: #{host}/#{variant}" unless group.map { |run| run[:config] }.uniq.length == 1
  case_lists = group.map { |run| run[:cases].map { |record| record[:id] } }
  abort "Cancellation cases changed: #{host}/#{variant}" unless case_lists.uniq.length == 1
  cases = group.flat_map { |run| run[:cases] }.group_by { |record| record[:id] }.map do |id, records|
    process_medians = records.map { |record| record[:processMedianMs] }
    samples = records.flat_map { |record| record[:samplesMs] }
    {
      id: id, processMediansMs: process_medians, medianOfMediansMs: median(process_medians),
      pooledMedianMs: median(samples), pooledMaxMs: samples.max, samples: samples.length
    }
  end
  { host: host, variant: variant, runs: group.length, cases: cases }
end
summary = {
  runs: runs, groups: groups,
  cancellationRuns: cancellation_runs, cancellationGroups: cancellation_groups
}
File.write(output, JSON.pretty_generate(summary) + "\n")
