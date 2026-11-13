#!/usr/bin/env ruby
# frozen_string_literal: true

require 'rbconfig'
require_relative 'bounded_process'

# Each host test owns its process: test-only prepends and retained VM stacks
# must not change another test's semantics or resource allowance.
failures = []
paths = Dir[File.expand_path('../test/*_test.rb', __dir__)].sort
paths.each do |path|
  result = JedNativeTools::BoundedProcess.run([RbConfig.ruby, path],
    timeout: ENV.fetch('JED_NATIVE_TIMEOUT', '180'),
    memory_bytes: ENV.fetch('JED_NATIVE_MEMORY_BYTES', (512 * 1024 * 1024).to_s),
    address_space_bytes: ENV.fetch('JED_NATIVE_ADDRESS_SPACE_BYTES', (1024 * 1024 * 1024).to_s))
  $stdout.write(result.stdout)
  $stderr.write(result.stderr)
  unless result.success?
    failures << File.basename(path)
    warn "#{File.basename(path)}: #{result.reason}"
  end
  $stdout.flush
end
puts "ruby-native host tests: #{paths.length - failures.length}/#{paths.length} files passed"
abort "failed: #{failures.join(', ')}" unless failures.empty?
