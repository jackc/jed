#!/usr/bin/env ruby
# frozen_string_literal: true

require 'fileutils'
require 'json'
require 'digest'
require 'optparse'
require 'shellwords'
require_relative 'shared_smoke_report'

root = File.expand_path('../..', __dir__)
results = File.join(__dir__, 'results')
build = File.join(root, '.dev/ruby-native-bench-build')
reference = File.join(results, 'shared-go-smoke.json')
output = File.join(results, 'shared-smoke-comparison.json')
attempts = []
compare_only = false
arguments = ARGV.empty? && ENV['usage_args'] ? Shellwords.split(ENV['usage_args']) : ARGV
OptionParser.new do |options|
  options.banner = 'Usage: shared_go_smoke.rb [--compare-only] [--attempt STATUS_PATH ...]'
  options.separator 'Supply attempts oldest to newest. Every result and its metadata are retained.'
  options.on('--compare-only', 'Use the saved canonical Go results without running SQL') { compare_only = true }
  options.on('--attempt PATH', 'Ruby .status.json file; repeat for each attempt') { |path| attempts << path }
  options.on('--reference PATH', 'Canonical Go smoke artifact') { |path| reference = path }
  options.on('--output PATH', 'Merged comparison artifact') { |path| output = path }
  options.on('-h', '--help', 'Show this help') { puts options; exit }
end.parse!(arguments)
abort "unexpected arguments: #{arguments.join(' ')}" unless arguments.empty?

unless compare_only
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
                                            workload_fingerprint: Digest::SHA256.file(File.join(root, 'bench/corpus/benchmarks.toml')).hexdigest,
                                            completed: okay, outcomes: outcomes) + "\n")
  abort 'canonical Go smoke failed; see logs and reference status' unless okay
end

default_attempt = File.join(results, 'shared-smoke.jsonl.status.json')
if attempts.empty? && !File.file?(default_attempt)
  warn 'Ruby smoke still pending; run this task with --compare-only after it finishes'
  exit
end
attempts << default_attempt if attempts.empty?
report = NativeRubySharedSmokeReport.build(reference: reference, attempts: attempts, root: root)
FileUtils.mkdir_p(File.dirname(output))
temporary = "#{output}.tmp"
File.write(temporary, JSON.pretty_generate(report) + "\n")
File.rename(temporary, output)
puts JSON.pretty_generate(report.fetch('summary'))
abort 'shared workload comparison is incomplete or has mismatches; see retained attempts and provenance' unless report.fetch('summary').fetch('all_canonical_workloads_match')
