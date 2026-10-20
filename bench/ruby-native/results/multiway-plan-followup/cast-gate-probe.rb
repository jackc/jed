# frozen_string_literal: true
require 'json'
ROOT = '/home/jack/.codex/worktrees/b8e6/jed'
require '/tmp/jed-multiway-plan-before/impl/ruby-native/lib/jed_native'
require File.join(ROOT, 'impl/ruby/lib/jed')
fixture = File.read(File.join(ROOT,'spec/conformance/suites/joins/safe_conjunct_pushdown.test')).scan(/^statement ok\n([^\n]+)/).first(8).flatten
queries = [
  ['identity parameter', 'SELECT a.id, b.w FROM a JOIN b ON a.id = b.aid WHERE a.id = CAST($1 AS i32) ORDER BY b.w', [2]],
  ['identity column', 'SELECT a.id, b.w FROM a JOIN b ON a.id = b.aid WHERE CAST(a.id AS i32) = $1 ORDER BY b.w', [2]],
  ['literal cast', 'SELECT a.id, b.w FROM a JOIN b ON a.id = b.aid WHERE a.id = CAST(2 AS i32) ORDER BY b.w', []],
  ['widening column', 'SELECT a.id, b.w FROM a JOIN b ON a.id = b.aid WHERE CAST(a.id AS i64) = $1 ORDER BY b.w', [2]]
]
reports = [['ruby-before',JedNative],['rust',Jed]].to_h do |label,core|
  database = core.memory
  session = core == JedNative ? database.session : database
  fixture.each { |sql| session.execute(sql) }
  reports = queries.to_h do |name,sql,params|
    result = session.query(sql,*params)
    rows = result.map(&:to_a)
    data = { rows: rows,cost: result.cost,columns:result.columns,types:result.column_types }
    result.close if result.respond_to?(:close)
    [name,data]
  end
  session.close if core == JedNative
  database.close
  [label,reports]
end
File.write('/tmp/jed-cast-gate-probe.json',JSON.pretty_generate(reports))
puts JSON.generate(reports)
