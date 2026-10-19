# frozen_string_literal: true

require_relative '../lib/jed_native'

# SQL values/errors are covered by the shared CTE/JOIN/JSON corpus. This checks
# the host-only contract that a cursor exposes correct metadata before a pull,
# including when an empty result will never have a row to repair its headers.
module NativeCursorMetadataTests
  module_function

  def assert(value, message) = (raise(message) unless value)

  class CountingBindings < JedNative::ParameterBindings
    attr_reader :resolutions
    private

    def resolve_expression(node, scope, wanted = nil)
      @resolutions = (@resolutions || 0) + 1
      super
    end
  end

  def run
    JedNative.memory do |database|
      session = database.session
      session.execute_script('CREATE TABLE a(id i32, x i32); CREATE TABLE b(id i32, y i32); INSERT INTO a VALUES(1,10); INSERT INTO b VALUES(1,20)')
      cases = [
        ['SELECT * FROM (SELECT 1 AS k, 5 AS id) JOIN (SELECT 1 AS id) USING(id)', %w[id k], %w[i64 i64]],
        ['SELECT o.w, id, a.id, b.id FROM (SELECT 1 AS w) o JOIN (a LEFT JOIN b USING(id)) ON true', %w[w id id id], %w[i64 i32 i32 i32]],
        ['SELECT g.* FROM (a RIGHT JOIN b USING(id)) AS g WHERE false', %w[id x y], %w[i32 i32 i32]],
        ['SELECT o.w, g.* FROM (SELECT 1 AS w) o, (a JOIN b USING(id)) AS g(key, ax, by)', %w[w key ax by], %w[i64 i32 i32 i32]],
        ["SELECT '{\"a\":1}'::jsonb - 'a' AS removed, array_to_json(ARRAY[1], 'false') AS image", %w[removed image], %w[jsonb json]]
      ]
      cases.each do |sql, names, types|
        statement = session.prepare(sql)
        2.times do
          eager = session.execute_prepared(statement)
          cursor = session.query_prepared(statement)
          assert(cursor.cost.zero?, "metadata evaluated query: #{sql}")
          assert(cursor.columns == names, "pre-pull names differ: #{sql}")
          assert(cursor.column_types == types, "pre-pull types differ: #{sql}")
          assert(cursor.rows == eager.rows && cursor.cost == eager.cost, "drained query differs: #{sql}")
          assert(cursor.columns == names && cursor.column_types == types, "drain changed metadata: #{sql}")
        end
      end
      [
        ['WITH RECURSIVE c AS (SELECT n FROM c) SELECT n FROM c', '42P19'],
        ['WITH RECURSIVE c(n) AS (SELECT n FROM c UNION ALL SELECT n + 1 FROM c) SELECT n FROM c', '42P19'],
        ['SELECT 1 FROM a RIGHT JOIN (b JOIN LATERAL (SELECT id) s ON true) ON true', '42P10']
      ].each do |sql, state|
        begin
          session.query(sql)
          raise "invalid cursor admitted: #{sql}"
        rescue JedNative::Error => error
          assert(error.sqlstate == state, "metadata error #{error.sqlstate} differs from #{state}")
        end
        assert(database.oldest_live_txid == database.version, 'failed metadata retained a cursor pin')
      end

      # A type hint must not re-walk each nested array twice per level. Keep
      # this assertion about internal work, independent of wall-clock speed.
      [20, 40].each do |depth|
        expression = '$1::i32'
        depth.times { expression = "(ARRAY[1::i32] || ARRAY[1::i32, #{expression}])[3]" }
        statement = session.prepare("SELECT #{expression} FROM a")
        binding = CountingBindings.new(session.engine, statement.ast)
        binding.resolve
        assert(binding.types == ['i32'], 'nested array hint changed parameter type')
        assert(binding.resolutions < depth * 100, "nested array metadata work grew too quickly: #{binding.resolutions}")
        [1, 5].each do |value|
          eager = session.execute_prepared(statement, value)
          cursor = session.query_prepared(statement, value)
          assert(cursor.column_types == ['i32'], 'nested array cursor metadata type')
          assert(cursor.rows == [[value]] && cursor.cost == eager.cost, 'nested array parameter result or cost')
        end
      end
      session.close
    end
    puts 'ruby-native cursor metadata: pre-pull and empty-result headers, cached bindings, JSON types, static errors and pins passed'
  end
end

NativeCursorMetadataTests.run
