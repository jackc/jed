# frozen_string_literal: true

require_relative 'compiled_text_test'

# SQL LIKE semantics live in the corpus. These probes compare generated kernels
# with the independent evaluator, and exercise template admission, host values,
# accounting prefixes, compiler consumers, and warmed public APIs.
module NativeCompiledLikeTests
  module_function

  Helpers = NativeCompiledTextTests
  COLUMNS = [{ name: 'a', type: 'text' }, { name: 'b', type: 'text' }, { name: 'id', type: 'i32' }].freeze
  MARKER = '# literal LIKE specialization'

  def assert(value, message) = Helpers.assert(value, message)
  def quoted(value) = "'#{value.gsub("'", "''")}'"

  def resolve(sql, **options)
    Helpers.resolve(sql, columns: COLUMNS, **options)
  end

  def build(sql, **options)
    expression, types = resolve(sql, **options)
    [expression, JedNative::CompiledExpression.build(expression, types: types)]
  end

  def compare(sql, values: [], **options)
    kernel = Helpers.direct_compare(sql, columns: COLUMNS, values: values, **options)
    assert(kernel.source.include?(MARKER), "LIKE specialization was not admitted: #{sql}")
    assert(kernel.pool.all? { |item| !item.is_a?(String) || item.frozen? }, 'LIKE pool retained mutable string data')
    kernel
  end

  def patterns
    patterns = ['', '%', '%%%%', 'a', 'a%', '%a', '%a%', '%%a%%', 'a\\%', '%a\\_%', '\\\\%', '\\a%', '😀%', '%é', "%a\u0000b%", "%a\nb%"]
    subjects = [nil, '', 'a', 'aa', 'ba', 'ab', 'a%', 'a_', '\\tail', '😀x', 'é', 'xé', "a\u0000b", "xa\nby"]
    patterns.each do |pattern|
      ['LIKE', 'NOT LIKE'].each do |operator|
        sql = "SELECT a #{operator} #{quoted(pattern.include?("\u0000") ? '%ab%' : pattern)}"
        expression, = resolve(sql)
        expression.args.last.value = pattern
        subjects.each { |subject| compare(sql, expression: expression, values: [subject]) }
      end
    end
    compare("SELECT a::text LIKE 'a%'", values: ['alpha'])
    compare("SELECT 'a'::text LIKE 'a%'", expected: [:value, true])
    compare("SELECT a LIKE 'a%'", values: ['a'], failure: 0, expected: [:error, 'XX001'])
    compare("SELECT a LIKE 'a%' AND 1 / 0 > 0", values: ['z'], expected: [:value, false])
    compare("SELECT a LIKE 'a%' AND 1 / 0 > 0", values: [nil], expected: [:error, '22012'])
    compare("SELECT 1 WHERE a LIKE 'a%' AND 1 / 0 > 0", values: [nil], filter: true, expected: [:value, nil])
    compare("SELECT a NOT LIKE 'a%' OR 1 / 0 > 0", values: ['z'], expected: [:value, true])
    compare("SELECT CASE WHEN a LIKE 'a%' THEN 7 ELSE 1 / 0 END", values: ['a'], expected: [:value, 7])
  end

  def rejection_and_bounds
    ["a LIKE 'a_b'", "a LIKE 'a%b'", "a LIKE '%a%b%'", "a LIKE 'a\\'", "a ILIKE 'a%'", "a NOT ILIKE 'a%'",
     "a LIKE b", "a LIKE b::text", "a LIKE 'a#%' ESCAPE '#'", "a LIKE NULL", "(a COLLATE \"C\") LIKE 'a%'"].each do |body|
      _, kernel = build("SELECT #{body}")
      assert(kernel.nil?, "unsupported LIKE expression compiled: #{body}")
    end
    _, kernel = build("SELECT $1::text LIKE 'a%'", types: ['text'])
    assert(kernel.nil?, 'unlabeled subject bypassed ordinary host string methods')
    expression, = resolve("SELECT a LIKE 'a%'")
    assert(JedNative::CompiledExpression.build(expression, types: {}, text: false).nil?, 'text-disable flag admitted LIKE')
    assert(JedNative::CompiledExpression.build(expression, types: { 'text' => [] }).nil?, 'nominal text type admitted LIKE')
    ['unicode', 'es'].each do |name|
      node, = resolve("SELECT 'a' LIKE 'a%'", environment: Helpers.unicode_environment, default: name)
      assert(JedNative::CompiledExpression.build(node, types: {}).nil?, 'loaded collation compiled as C LIKE')
    end
    [JedNative::CollationLabel.new(conflict: true), JedNative::CollationLabel.new(none: true)].each do |label|
      node, = resolve("SELECT a LIKE 'a%'")
      node.args.first.instance_variable_set(:@native_collation, label)
      assert(JedNative::CompiledExpression.build(node, types: {}).nil?, 'indeterminate child label admitted LIKE')
    end

    cap = JedNative::CompiledExpression::MAX_POOL_BYTES
    _, accepted = build("SELECT a LIKE #{quoted('a' * (cap / 2))}")
    assert(accepted && accepted.pool.grep(String).sum(&:bytesize) == cap, 'LIKE did not account original and decoded bytes at exact cap')
    _, rejected = build("SELECT a LIKE #{quoted('a' * (cap / 2 + 1))}")
    assert(rejected.nil?, 'LIKE exceeded cumulative pool byte cap')
    _, rejected = build("SELECT a LIKE #{quoted('a' * (cap + 1))}")
    assert(rejected.nil?, 'oversized LIKE pattern escaped preparation bound')
    _, accepted = build("SELECT a LIKE #{quoted('😀' * (cap / 8))}")
    assert(accepted && accepted.pool.grep(String).sum(&:bytesize) == cap, 'LIKE measured Unicode characters instead of bytes')
    rejected_expr, = resolve("SELECT coalesce(a, 'x') LIKE 'a%'")
    accepted_expr, = resolve("SELECT a LIKE #{quoted('a' * (cap / 2))}")
    projection = JedNative::CompiledExpression.build_many([rejected_expr, accepted_expr], types: {})
    assert(projection && projection.pool.grep(String).sum(&:bytesize) == cap, 'unsupported LIKE root retained partial pool bytes')
    assert(JedNative::CompiledExpression.prepare_many([rejected_expr, accepted_expr], types: {}).nil?, 'complete arguments admitted a partial LIKE fallback')

    attack = "\"); raise('owned'); #"
    kernel = compare("SELECT a LIKE #{quoted(attack)}", values: [attack], expected: [:value, true])
    assert(!kernel.source.include?(attack) && kernel.pool.include?(attack), 'LIKE literal became Ruby source')
  end

  class BytesWithChars < JedNative::Bytea
    def chars = ['a', 'x']
  end

  class TextWithChars < String
    def chars = ['z']
    def start_with?(_) = raise('custom prefix method escaped normalization')
  end

  def host_values
    unicode = Helpers.unicode_environment.collations.fetch('unicode')
    labels = [JedNative::CollationLabel.new(name: 'unicode', table: unicode),
              JedNative::CollationLabel.new(conflict: true), JedNative::CollationLabel.new(none: true)]
    values = [TextWithChars.new('ax'), BytesWithChars.new('xx'), JedNative::Bytea.new('ax'),
              'ax'.encode(Encoding::UTF_16LE), 'ax'.b, "\xffa".b.force_encoding(Encoding::UTF_8)]
    labels.each { |label| values << JedNative::CollatedText.new('ax', label) }
    [String.new('ax'), JedNative::CollatedText.new('ax', labels.first)].each do |value|
      value.define_singleton_method(:chars) { raise 'singleton chars escaped C normalization' }
      value.define_singleton_method(:start_with?) { |_| raise 'singleton prefix escaped C normalization' }
      value.freeze
      values << value
    end
    values.each do |value|
      ['a%', '%x', '%a%', 'ax', '%', ''].each do |pattern|
        compare("SELECT a LIKE #{quoted(pattern)}", values: [value])
        compare("SELECT a NOT LIKE #{quoted(pattern)}", values: [value])
      end
    end
    # Static subject labels overwrite runtime labels, just as the interpreter
    # does. A retained kernel observes new engines/rows, never prior contexts.
    expression, kernel = build("SELECT a LIKE 'a%'")
    3.times do |index|
      subject = index.even? ? 'alpha' : 'beta'
      engine = Helpers.engine
      assert(kernel.call(engine, [subject], []) == engine.evaluate(expression, [subject]), 'LIKE retained prior execution values')
    end
    assert(kernel.pool.all? { |item| !item.is_a?(JedNative::Engine) && !item.is_a?(JedNative::Expression) }, 'LIKE pool retained execution context')
  end

  def consumers
    expression, = resolve("SELECT a LIKE 'a%'")
    rows = [['alpha', nil, 1], ['beta', nil, 2], [nil, nil, 3]]
    engine = Helpers.engine
    reference = rows.map { |row| engine.evaluate(expression, row) }
    producers = [JedNative::CompiledExpression.prepare_many([expression], types: {})&.materialize,
                 JedNative::CompiledRelation.prepare_keys([expression], types: {})&.materialize,
                 JedNative::CompiledRelation.prepare_filter(expression, types: {})&.materialize,
                 JedNative::CompiledPipeline.build(nil, [expression], types: {}, mode: :array),
                 JedNative::CompiledPipeline.build(nil, [expression], types: {}, mode: :stream)]
    assert(producers.all? { |kernel| kernel && kernel.source.include?(MARKER) }, 'a compiler consumer rejected complete LIKE')
    assert(rows.map { |row| producers[0].call(Helpers.engine, row, []) } == reference.map { |value| [value] }, 'argument LIKE differs')
    assert(producers[1].call(Helpers.engine, rows, []) == reference.map { |value| [value] }, 'group-key LIKE differs')
    assert(producers[2].call(Helpers.engine, rows, []) == [rows.first], 'relation-filter LIKE differs')
    producers.drop(3).each { |kernel| assert(kernel.call(Helpers.engine, rows, [], []) == reference.map { |value| [value] }, 'pipeline LIKE differs') }
    aggregate, = resolve("SELECT count(*) FILTER (WHERE a LIKE 'a%')")
    kernel = JedNative::CompiledAggregate.build(aggregate, types: {})
    assert(kernel && kernel.source.include?(MARKER), 'aggregate input rejected complete LIKE')
    assert(kernel.call(Helpers.engine, rows, []) == Helpers.engine.compute_aggregate(aggregate, rows), 'aggregate input LIKE differs')
  end

  def api_equivalence(database)
    session = database.session
    session.execute_script("CREATE TABLE patterns(id i32 PRIMARY KEY, label text); INSERT INTO patterns VALUES (1, 'alpha'), (2, 'beta'), (3, NULL), (4, '😀alpha')")
    queries = ["SELECT id FROM patterns WHERE label LIKE 'a%' ORDER BY id", "SELECT id, label NOT LIKE '%a%' FROM patterns ORDER BY id",
               "SELECT CASE WHEN label LIKE '%alpha' THEN 1 ELSE 0 END, count(*) FROM patterns GROUP BY CASE WHEN label LIKE '%alpha' THEN 1 ELSE 0 END ORDER BY 1",
               "SELECT bool_or(label LIKE 'a%'), count(*) FILTER (WHERE label LIKE '%a') FROM patterns",
               "SELECT id FROM patterns WHERE label LIKE 'a%' AND id >= $1 ORDER BY id", "SELECT id FROM patterns WHERE label LIKE 'a_%' ORDER BY id"]
    queries.each do |sql|
      args = sql.include?('$1') ? [1] : []
      statement = session.prepare(sql)
      session.engine.instance_variable_set(:@native_compilation_disabled, true)
      baseline = session.execute_prepared(statement, *args)
      session.engine.instance_variable_set(:@native_compilation_disabled, false)
      session.engine.instance_variable_set(:@native_compilation_immediate, true)
      3.times do
        [:execute, :execute_prepared, :query, :query_prepared].each do |api|
          result = session.public_send(api, api.to_s.end_with?('prepared') ? statement : sql, *args)
          values = api.to_s.start_with?('query') ? result.each.to_a : result.rows
          assert(values == baseline.rows && result.columns == baseline.columns && result.column_types == baseline.column_types && result.cost == baseline.cost, "warmed #{api} LIKE changed values, metadata or cost")
        end
      end
      session.engine.instance_variable_set(:@native_text_compilation_disabled, true)
      assert(session.query_prepared(statement, *args).rows == baseline.rows, 'disabling warmed text kernels changed LIKE')
      session.engine.instance_variable_set(:@native_text_compilation_disabled, false)
    end
    statement = session.prepare("SELECT id FROM patterns WHERE label LIKE 'a%' ORDER BY id")
    cursor = session.query_prepared(statement)
    session.execute("INSERT INTO patterns VALUES (5, 'after')")
    assert(cursor.rows == [[1]] && session.query_prepared(statement).rows == [[1], [5]], 'LIKE reused rows across captured publications')
    session.execute('ALTER TABLE patterns ADD COLUMN extra i32')
    assert(session.query_prepared(statement).rows == [[1], [5]], 'LIKE reused an invalid schema plan')
  ensure
    cursor&.close
    session&.close
  end

  def run
    patterns
    rejection_and_bounds
    host_values
    consumers
    JedNative.memory { |database| api_equivalence(database) }
    Dir.mktmpdir('jed-compiled-like-') do |directory|
      JedNative.create(File.join(directory, 'patterns.jed'), locking: :exclusive) { |database| api_equivalence(database) }
    end
    puts 'ruby-native compiled LIKE: bounded literal shapes, escaping, independent costs/errors, host guards, compiler consumers and warmed APIs passed'
  end
end

NativeCompiledLikeTests.run
