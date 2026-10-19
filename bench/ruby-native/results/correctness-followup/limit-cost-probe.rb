require 'json'
require_relative '../../../../impl/ruby-native/lib/jed_native'
require_relative '../../../../impl/ruby/lib/jed'

schema = 'CREATE TABLE items (id i32 PRIMARY KEY, bucket i32, score i32, label text)'
values = 1000.times.map { |i| "(#{i},#{i % 10},#{(i * 37) % 10_000},'row#{i}')" }.join(',')
queries = [
  'SELECT id, score * 3 + bucket FROM items WHERE bucket >= $1 ORDER BY id LIMIT 20',
  'SELECT id, score * 3 + bucket FROM items WHERE bucket >= $1 ORDER BY id LIMIT 20 OFFSET 500'
]
report = {}
native = JedNative.memory
session = native.session
wrapper = Jed.memory
[session, wrapper].each { |s| s.execute(schema); s.execute("INSERT INTO items VALUES #{values}") }
queries.each do |sql|
  expected = 5.times.map { |i| result = wrapper.query(sql, i); [result.values, result.cost] }
  rows = %i[execute query execute_prepared query_prepared].to_h do |api|
    prepared = session.prepare(sql)
    actual = 5.times.map do |i|
      result = api.to_s.end_with?('_prepared') ? session.public_send(api, prepared, i) : session.public_send(api, sql, i)
      [result.rows, result.cost]
    end
    raise "#{api}: #{sql}" unless actual == expected
    [api, actual.map(&:last)]
  end
  report[sql] = rows.merge(wrapper: expected.map(&:last))
end
puts JSON.pretty_generate(report)
session.close
native.close
wrapper.close
