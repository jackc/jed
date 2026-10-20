# frozen_string_literal: true
require 'json'
require 'digest'
require 'time'
require File.join(Dir.pwd, 'scripts/lib/pg_oracle')

started = Time.now.utc.iso8601(6)
paths = %w[spec/conformance/suites/params/binding.test spec/conformance/suites/joins/safe_conjunct_pushdown.test spec/conformance/oracle_profile.toml]
pin = -> { paths.to_h { |path| [path, Digest::SHA256.file(path).hexdigest] } }
before = pin.call
oracle = PgOracle.new(label: 'multiway parameter width regressions')
oracle.add_applied('CREATE TABLE narrow (id i32 PRIMARY KEY, s i16)')
oracle.add_applied('INSERT INTO narrow VALUES (1,100)')
oracle.add_applied('CREATE TABLE narrow_key (id i16 PRIMARY KEY)')
oracle.add_applied('INSERT INTO narrow_key VALUES (-1),(1),(100)')
jobs = [
  ['SELECT id FROM narrow WHERE s = $1 AND $1 = $2', '40000,40000'],
  ['SELECT id FROM narrow WHERE s = $1 AND $1 = $2', '100,100'],
  ['SELECT id FROM narrow WHERE s = $1 OR id = $1', '40000'],
  ['SELECT id FROM narrow WHERE id = $1 OR $1 = 5', '2147483648'],
  ['SELECT id FROM narrow_key WHERE id = $1 AND $1 < 100000', '40000'],
  ['SELECT id FROM narrow_key WHERE id = $1 AND $1 < 100000', '-40000'],
  ['SELECT $1::i64 FROM narrow_key WHERE id = $1', '9223372036854775807'],
  ['SELECT s + $1, $1 + 1 FROM narrow WHERE id = $1', '1'],
  ['SELECT $1 + 1, s + $1 FROM narrow WHERE id = $1', '1'],
  ['SELECT $1::i16, $1::i32, $1::i64', '1'],
  ['SELECT s + $1, $1::i64 FROM narrow', '32700']
]
observations = jobs.map do |sql, params|
  body = "\\set VERBOSITY sqlstate\nPREPARE p AS #{oracle.pg_sql(sql)};\nEXECUTE p(#{params});"
  output, error = oracle.run(body)
  {sql: sql, params: params, output: output, stderr: error, sqlstates: error.scan(/ERROR:\s+([0-9A-Z]{5})/).flatten}
end
after = pin.call
raise 'corpus/profile changed during oracle probe' unless before == after
puts JSON.pretty_generate({started_at: started, finished_at: Time.now.utc.iso8601(6), runtime: RUBY_DESCRIPTION,
  profile: PgOracle.profile, live_cluster: PgOracle.live_cluster,
  harness_sha256: Digest::SHA256.file(__FILE__).hexdigest,
  source_sha256: before, source_sha256_after: after, observations: observations})
