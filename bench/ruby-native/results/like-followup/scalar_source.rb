# frozen_string_literal: true

# Non-timing evidence for the unchanged scalar control's emitted Ruby/pool.
require_relative '../../compiled_diagnostics'
source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE'))
before = NativeRubySource.digest(source)
require File.join(source, 'lib/jed_native')
engine = JedNative::Engine.new
engine.instance_variable_set(:@native_compilation_disabled, true)
engine.instance_variable_set(:@params, [-999])
sql = 'abs($1::i64) + 7'
resolved = engine.compile(JedNative::Parser.new(sql).parse_expression, [])
kernel = JedNative::CompiledExpression.build(resolved, types: engine.instance_variable_get(:@types))
raise 'scalar control did not compile' unless kernel
outcomes = [-999, 0, 994, nil].map do |value|
  ordinary = CompiledExpressionDiagnostics.outcome(engine, resolved, nil, [], [value])
  compiled = CompiledExpressionDiagnostics.outcome(engine, resolved, kernel, [], [value])
  raise 'scalar control differs' unless ordinary == compiled
  { params: [value], outcome: compiled }
end
report = { source_sha256: before, final_source_sha256: NativeRubySource.digest(source),
  script_sha256: Digest::SHA256.file(__FILE__).hexdigest, ruby: RUBY_DESCRIPTION, executable: RbConfig.ruby,
  sql: sql, type: resolved.type, generated_source: kernel.source,
  generated_source_sha256: Digest::SHA256.hexdigest(kernel.source), pool: kernel.pool, outcomes: outcomes }
raise 'source changed' unless report[:source_sha256] == report[:final_source_sha256]
abort 'Specify one report path' unless ARGV.length == 1
File.write(ARGV.first, JSON.pretty_generate(report) + "\n")
puts JSON.generate(report)
