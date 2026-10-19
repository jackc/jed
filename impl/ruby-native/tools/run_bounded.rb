#!/usr/bin/env ruby
# frozen_string_literal: true
require_relative 'bounded_process'
require 'digest'
abort 'usage: run_bounded.rb command [argument ...]' if ARGV.empty?
source_root = ENV.fetch('RUBY_NATIVE_SOURCE', File.expand_path('..', __dir__))
source_digest = lambda do
  digest = Digest::SHA256.new
  Dir[File.join(source_root, 'lib/**/*.rb')].sort.each { |path| digest.update(File.binread(path)) }
  digest.hexdigest
end
before = source_digest.call
started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
timeout = Float(ENV.fetch('JED_NATIVE_TIMEOUT', '60'))
result = JedNativeTools::BoundedProcess.run(ARGV,
  timeout: timeout,
  memory_bytes: ENV.fetch('JED_NATIVE_MEMORY_BYTES', (512 * 1024 * 1024).to_s),
  address_space_bytes: ENV.fetch('JED_NATIVE_ADDRESS_SPACE_BYTES', (1024 * 1024 * 1024).to_s))
$stdout.write(result.stdout)
$stderr.write(result.stderr)
metadata = { command: ARGV, success: result.success?, reason: result.reason,
             peak_rss_bytes: result.peak_rss_bytes, memory_bytes: result.memory_bytes,
             address_space_bytes: result.address_space_bytes, timeout_seconds: timeout,
             elapsed_seconds: Process.clock_gettime(Process::CLOCK_MONOTONIC) - started,
             source: source_root, source_sha256: before, final_source_sha256: source_digest.call,
             runtime: RUBY_DESCRIPTION,
             environment: %w[JED_PROCESS_CORES JED_PROCESS_JOBS JED_PROCESS_FILTER].to_h { |key| [key, ENV[key]] }.compact,
             stdout: result.stdout, stderr: result.stderr }
File.write(ENV['JED_NATIVE_RUN_REPORT'], JSON.pretty_generate(metadata) + "\n") if ENV['JED_NATIVE_RUN_REPORT']
$stderr.puts "bounded worker: #{result.reason || 'passed'}; peak RSS #{result.peak_rss_bytes} bytes"
exit(result.success? ? 0 : 1)
