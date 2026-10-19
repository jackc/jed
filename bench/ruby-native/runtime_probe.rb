#!/usr/bin/env ruby
# frozen_string_literal: true

# Run with the runtime/JIT being investigated, outside timed benchmark samples.
require 'json'
require 'rbconfig'
require 'openssl'
require 'zlib'
require_relative '../../impl/ruby-native/lib/jed_native'

yjit = defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?
zjit = defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?
mode = zjit ? 'zjit' : yjit ? 'yjit' : 'interpreter'
expected_mode = ARGV.fetch(0)
abort "expected #{expected_mode}, got #{mode}" unless mode == expected_mode

checks = 0
JedNative.memory do |database|
  database.execute('CREATE TABLE items (id i32 PRIMARY KEY, value i32)')
  database.execute("INSERT INTO items VALUES #{100.times.map { |id| "(#{id},#{id * 3})" }.join(',')}")
  statement = database.prepare('SELECT value FROM items WHERE id = $1')
  1_000.times do |i|
    id = i % 100
    actual = database.query_prepared(statement, id).rows.map { |row| row[0] }
    abort "incorrect primary-key result: #{actual.inspect}" unless actual == [id * 3]
    checks += 1
  end
end

stats = if zjit
          RubyVM::ZJIT.stats || {}
        elsif yjit
          RubyVM::YJIT.runtime_stats || {}
        else
          {}
        end
if mode != 'interpreter'
  abort "#{mode} did not compile any methods" unless stats.fetch(:compiled_iseq_count, 0).positive?
end
puts JSON.pretty_generate({
  ruby: RUBY_DESCRIPTION, executable: RbConfig.ruby,
  configure_args: RbConfig::CONFIG.fetch('configure_args'),
  mode: mode, yjit_enabled: !!yjit, zjit_enabled: !!zjit,
  primary_key_checks: checks, openssl: OpenSSL::OPENSSL_VERSION,
  zlib: Zlib::ZLIB_VERSION,
  compilation: stats.select { |key, _| key.to_s.match?(/\A(?:compiled_|failed_iseq|compile_time|code_region|inline_code)/) }
})
