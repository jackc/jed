# frozen_string_literal: true

require_relative '../lib/jed_native'
require 'tmpdir'
module NativeHostBatchTests
  module_function
  def outcome(session, sql, budget = 0)
    session.max_cost = budget
    result = session.execute(sql)
    [result.rows, result.cost]
  rescue JedNative::Error => error
    [error.sqlstate, session.engine.cost]
  ensure
    session.max_cost = 0
  end
  def run
    batches = []
    scalar = JedNative::Environment.new.register_function(name: 'host_map', args: ['i64'], returns: 'i64', cost: 3, volatility: :stable) do |value|
      raise JedNative::Error.new('22012', 'host row failure') if value == 77
      value + 1
    end
    batch = JedNative::Environment.new.register_function(name: 'host_map', args: ['i64'], returns: 'i64', cost: 3, volatility: :stable,
      batch_kernel: lambda do |columns, output|
        batches << columns[0].length
        columns[0].each do |value|
          raise JedNative::Error.new('22012', 'host row failure') if value == 77
          output << value + 1
        end
      end)
    Dir.mktmpdir('ruby-host-batches') do |directory|
      databases = [scalar, batch].each_with_index.map do |environment, index|
        database = JedNative.create(File.join(directory, "#{index}.jed"), environment: environment, page_size: 256)
        database.execute('create table t(id i64 primary key,v i64)')
        database.execute('insert into t values ' + (1..160).map { |value| "(#{value},#{value == 9 ? 'NULL' : value})" }.join(','))
        database
      end
      sessions = databases.map(&:session)
      queries = [
        'select host_map(v) from t where id<70',
        'select host_map(v) from t order by id limit 70',
        'select host_map(v) from t where host_map(v)>20 order by id limit 30 offset 4',
        'select host_map(v) from t order by v desc limit 90',
        'select 1/(v-76),host_map(v) from t order by id',
        'select case when id<5 then host_map(v) else 0 end from t order by id',
        'select host_map(v) from t order by id limit 1'
      ]
      queries.each do |sql|
        [0, 1, 10, 70, 300].each do |budget|
          expected, actual = sessions.map { |session| outcome(session, sql, budget) }
          raise "batch replay differs #{budget} #{sql}: #{expected.inspect} vs #{actual.inspect}" unless expected == actual
        end
      end
      raise 'eligible host call was never batched' unless batches.any? { |size| size > 1 }
      raise 'streaming host batch exceeded its bound' unless batches.max <= 1024
      sessions.each(&:close)
      databases.each(&:close)
    end
    puts 'ruby-native host batches: scalar-equivalent rows, costs, aborts and error order across scan/sort/buffer paths passed'
  end
end
NativeHostBatchTests.run
