#!/usr/bin/env ruby
# frozen_string_literal: true

require 'fileutils'
require 'json'
require 'digest'

root = File.expand_path('../..', __dir__)
results = File.join(__dir__, 'results')
build = File.join(root, '.dev/ruby-native-bench-build')
reference = File.join(results, 'shared-go-smoke.json')

unless ARGV.include?('--compare-only')
  source_data = File.join(root, '.dev/ruby-native-bench-data')
  data = File.join(root, '.dev/ruby-native-go-smoke-data')
  FileUtils.mkdir_p([build, data, results])
  Dir[File.join(source_data, '*')].each do |path|
    next unless File.file?(path)

    FileUtils.cp(path, File.join(data, File.basename(path)))
  end
  replacements = {
    'bench/go/cmd/bench-jed/main.go' => {
      'Environment: env, Synchronous: synchronous}' => 'Environment: env, Synchronous: synchronous, Locking: jed.LockingExclusive}',
      'jed.OpenOptions{Environment: env}' => 'jed.OpenOptions{Environment: env, Locking: jed.LockingExclusive}'
    },
    'bench/go/internal/bench/runner.go' => {
      'b := &corpus.Bench[i]' => "b := &corpus.Bench[i]\n\t\tb.Warmup, b.Iterations = 0, 1"
    }
  }
  overlay = {}
  replacements.each_with_index do |(relative, edits), index|
    original = File.join(root, relative)
    source = File.read(original)
    edits.each do |before, after|
      raise "canonical harness changed: missing #{before}" unless source.include?(before)

      source = source.sub(before, after)
    end
    adapted = File.join(build, "smoke-#{index}.go")
    File.write(adapted, source)
    overlay[original] = adapted
  end
  overlay_path = File.join(build, 'smoke-overlay.json')
  File.write(overlay_path, JSON.generate('Replace' => overlay))
  binary = File.join(build, 'bench-go-smoke')
  environment = { 'GOCACHE' => File.join(build, 'cache'), 'GOPROXY' => 'off', 'GOSUMDB' => 'off' }
  unless system(environment, 'go', '-C', File.join(root, 'bench/go'), 'build', '-overlay', overlay_path,
                '-o', binary, './cmd/bench-jed')
    abort 'offline canonical Go smoke build failed'
  end
  raw = File.join(build, 'go-smoke-raw.jsonl')
  okay = system(binary, File.join(root, 'bench/corpus'), data, raw)
  outcomes = File.file?(raw) ? File.readlines(raw).map do |line|
    result = JSON.parse(line)
    result.slice('bench', 'dataset', 'iterations', 'warmup', 'rows_total', 'checksum', 'fingerprint')
  end : []
  File.write(reference, JSON.pretty_generate(mode: 'single_execution_smoke_no_timings',
                                            completed: okay, outcomes: outcomes) + "\n")
  abort 'canonical Go smoke failed; see logs and reference status' unless okay
end

ruby_path = File.join(results, 'shared-smoke.jsonl.status.json')
if File.file?(ruby_path)
  expected = JSON.parse(File.read(reference)).fetch('outcomes').to_h { |entry| [entry.fetch('bench'), entry] }
  comparisons = JSON.parse(File.read(ruby_path)).map do |entry|
    next entry unless entry['status'] == 'smoke_pass'

    other = expected[entry.fetch('bench')]
    same = other && entry.values_at('checksum', 'rows') == other.values_at('checksum', 'rows_total')
    entry.merge('status' => same ? 'matches_go' : 'mismatch_go',
                'go_checksum' => other&.fetch('checksum'), 'go_rows' => other&.fetch('rows_total'))
  end
  out = File.join(results, 'shared-smoke-comparison.json')
  File.write(out, JSON.pretty_generate(comparisons) + "\n")
  puts comparisons.group_by { |entry| entry.fetch('status') }.transform_values(&:length).inspect
  abort 'shared workload checksum mismatch' if comparisons.any? { |entry| entry['status'] == 'mismatch_go' }
else
  warn 'Ruby smoke still pending; run this task with --compare-only after it finishes'
end
