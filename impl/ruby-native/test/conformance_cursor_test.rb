# frozen_string_literal: true

require_relative '../tools/bounded_process'

# The adapter is a host, not a materializing engine API. Load its instrumentation
# only in a child so this host test does not modify the other tests' engines.
result = JedNativeTools::BoundedProcess.run(timeout: 30, memory_bytes: 512 * 1024**2,
  address_space_bytes: 2 * 1024**3) do
  require_relative '../bin/conformance'
  assert = ->(condition, message) { raise message unless condition }
  harness = NativeCorpus.new

  cursor_class = Class.new do
    include Enumerable
    attr_reader :closed
    def initialize(fail: false)
      @fail, @finished = fail, false
    end
    def each
      return enum_for(:each) unless block_given?
      yield [7]
      raise JedNative::Error.new('22012', 'deferred failure') if @fail
      @finished = true
    end
    def rows = raise('adapter called the engine-owned collector')
    def columns = [@finished ? 'final' : 'initial']
    def column_types = ['i32']
    def cost = @finished ? 37 : 1
    def rows_affected = nil
    def close = (@closed = true)
  end
  session_class = Class.new do
    def initialize(cursor) = (@cursor = cursor)
    def query(*) = @cursor
    def execute(*) = raise('query adapter routed to execute')
  end
  cursor = cursor_class.new
  output = harness.run_statement(session_class.new(cursor), 'query', 'SELECT 7', [])
  assert.call(output.rows == [[7]] && output.columns == ['final'] &&
    output.column_types == ['i32'] && output.cost == 37 && cursor.closed,
    'external drain lost rows, final metadata/cost or close')
  cursor = cursor_class.new(fail: true)
  begin
    harness.run_statement(session_class.new(cursor), 'query', 'SELECT 7', [])
    raise 'adapter swallowed deferred failure'
  rescue JedNative::Error => error
    assert.call(error.sqlstate == '22012' && cursor.closed, 'deferred failure lost its error or close')
  end

  JedNative.memory do |database|
    session = database.session
    session.execute('CREATE TABLE adapter_rows (id i32 PRIMARY KEY)')
    session.execute('INSERT INTO adapter_rows SELECT g FROM generate_series(1, 100) g')
    sql = 'SELECT id FROM adapter_rows ORDER BY id LIMIT 1000'
    control = session.query(sql)
    control.each {}
    expected_cost = control.cost
    session.max_query_memory_bytes = 1
    output = harness.run_statement(session, 'query', sql, [])
    assert.call(output.rows == (1..100).map { |i| [i] } && output.columns == ['id'] &&
      output.column_types == ['i32'] && output.cost == expected_cost,
      'external corpus collection changed results or its raw cursor allowance')
    begin
      session.query(sql).rows
      raise 'owned collector no longer enforces its allowance'
    rescue JedNative::Error => error
      assert.call(error.sqlstate == '54P05', 'owned collector raised the wrong error')
    end
    assert.call(database.instance_variable_get(:@pins).empty? &&
      session.instance_variable_get(:@cursors).empty?, 'adapter or collector retained a cursor pin')
    session.close
  end
  true
end
raise "conformance cursor adapter test failed: #{result.reason}\n#{result.stderr}" unless result.success? && result.value
puts 'ruby-native conformance cursor: external ownership, deferred errors, final metadata/cost and close passed'
