# frozen_string_literal: true

require_relative '../lib/jed_native'

# The shared SQL corpus uses 8 KiB pages, which rarely force compression.
# Mirror the native compressed_cost host cases at page_size 256 (capacity 240)
# to pin logical compression attempts and the touched-column scan charges.
module RubyCompressedCostTest
  module_function

  def assert_equal(expected, actual, message)
    raise "#{message}: expected #{expected}, got #{actual}" unless expected == actual
  end

  def with_database
    database = JedNative.memory(page_size: 256)
    session = database.session
    yield session
  ensure
    session&.close
    database&.close
  end

  def cost(session, sql) = session.execute(sql).cost

  def stream_cost(session, sql)
    rows = session.query(sql)
    rows.each { |_| }
    rows.cost
  ensure
    rows&.close
  end

  def filler_text(length)
    alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/'
    state = 0x4A454442
    Array.new(length) do
      state ^= (state << 13) & 0xFFFF_FFFF
      state ^= state >> 17
      state ^= (state << 5) & 0xFFFF_FFFF
      alphabet[state % 64]
    end.join
  end

  def compressed_tables
    with_database do |session|
      session.execute('CREATE TABLE comp (id i32 PRIMARY KEY, body text)')
      session.execute("INSERT INTO comp VALUES (1, '#{'x' * 600}'), (2, 'small')")
      session.execute('CREATE TABLE control (id i32 PRIMARY KEY, body text)')
      # Both row-1 values compare at least five bytes against the correlated
      # probe's 'small', keeping that delta solely the decompression charge.
      session.execute("INSERT INTO control VALUES (1, 'plain'), (2, 'small')")
      yield session
    end
  end

  def scan_decompression
    compressed_tables do |session|
      assert_equal(3, cost(session, 'SELECT * FROM comp') - cost(session, 'SELECT * FROM control'), 'full scan slabs')
    end
  end

  def external_compressed_scan
    with_database do |session|
      mixed = filler_text(200) + 'y' * 200
      session.execute('CREATE TABLE comp (id i32 PRIMARY KEY, body text)')
      session.execute("INSERT INTO comp VALUES (1, '#{mixed}')")
      session.execute('CREATE TABLE control (id i32 PRIMARY KEY, body text)')
      session.execute("INSERT INTO control VALUES (1, 'tiny')")
      # One compressed chain page plus ceil(400/240) = two raw slabs.
      assert_equal(3, cost(session, 'SELECT * FROM comp') - cost(session, 'SELECT * FROM control'), 'external-compressed scan')
    end
  end

  def bounded_scan_and_limit
    compressed_tables do |session|
      assert_equal(3, cost(session, 'SELECT * FROM comp WHERE id = 1') - cost(session, 'SELECT * FROM control WHERE id = 1'), 'compressed point lookup')
      assert_equal(3, stream_cost(session, 'SELECT * FROM comp WHERE id = 1') - stream_cost(session, 'SELECT * FROM control WHERE id = 1'), 'streaming compressed point lookup')
      assert_equal(0, cost(session, 'SELECT * FROM comp WHERE id = 2') - cost(session, 'SELECT * FROM control WHERE id = 2'), 'inline point lookup')
      assert_equal(3, cost(session, 'SELECT * FROM comp LIMIT 1') - cost(session, 'SELECT * FROM control LIMIT 1'), 'LIMIT keeps up-front slabs')
    end
  end

  def insert_attempts
    with_database do |session|
      session.execute('CREATE TABLE t (id i32 PRIMARY KEY, body text)')
      assert_equal(0, cost(session, "INSERT INTO t VALUES (1, 'small')"), 'inline insert')
      assert_equal(3, cost(session, "INSERT INTO t VALUES (2, '#{'x' * 600}')"), 'adopted compression attempt')
      assert_equal(3, cost(session, "INSERT INTO t VALUES (3, '#{filler_text(600)}')"), 'rejected compression attempt')
    end
  end

  def update_attempts
    compressed_tables do |session|
      big = cost(session, "UPDATE comp SET body = '#{'x' * 600}' WHERE id = 1")
      small = cost(session, "UPDATE comp SET body = 'small' WHERE id = 1")
      assert_equal(3, big - small, 'UPDATE fresh-row compression attempt')
    end
  end

  def alter_add_attempts
    compressed_tables do |session|
      read = cost(session, 'SELECT * FROM comp') - cost(session, 'SELECT * FROM control')
      comp = cost(session, 'ALTER TABLE comp ADD extra i32')
      control = cost(session, 'ALTER TABLE control ADD extra i32')
      assert_equal(3, read, 'pre-ALTER ADD scan')
      assert_equal(read + 3, comp - control, 'ALTER ADD reads and recompresses each rewritten row')
    end
  end

  def alter_drop_attempts
    compressed_tables do |session|
      session.execute('ALTER TABLE comp ADD extra i32')
      session.execute('ALTER TABLE control ADD extra i32')
      read = cost(session, 'SELECT * FROM comp') - cost(session, 'SELECT * FROM control')
      comp = cost(session, 'ALTER TABLE comp DROP extra')
      control = cost(session, 'ALTER TABLE control DROP extra')
      assert_equal(3, read, 'pre-ALTER DROP scan')
      assert_equal(read + 3, comp - control, 'ALTER DROP reads and recompresses each rewritten row')
    end
  end

  def decimal_payload
    with_database do |session|
      digits = '12' * 400 + '.5'
      session.execute('CREATE TABLE t (id i32 PRIMARY KEY, d numeric)')
      # The 801-digit coefficient occupies 201 base-10000 groups: 407 raw
      # payload bytes, or ceil(407/240) = two slabs in either direction.
      assert_equal(2, cost(session, "INSERT INTO t VALUES (1, #{digits})"), 'decimal compression attempt')
      session.execute('CREATE TABLE control (id i32 PRIMARY KEY, d numeric)')
      session.execute('INSERT INTO control VALUES (1, 7)')
      assert_equal(2, cost(session, 'SELECT * FROM t') - cost(session, 'SELECT * FROM control'), 'decimal decompression slabs')
    end
  end

  def untouched_columns
    compressed_tables do |session|
      ['SELECT id FROM', 'SELECT count(*) FROM'].each do |prefix|
        assert_equal(0, cost(session, "#{prefix} comp") - cost(session, "#{prefix} control"), "untouched body: #{prefix}")
      end
      assert_equal(3, cost(session, 'SELECT min(body) FROM comp') - cost(session, 'SELECT min(body) FROM control'), 'aggregate argument touches body')
    end
  end

  def correlated_touch
    compressed_tables do |session|
      session.execute('CREATE TABLE probe (id i32 PRIMARY KEY, body text)')
      session.execute("INSERT INTO probe VALUES (1, 'small')")
      comp = cost(session, 'SELECT id FROM comp WHERE EXISTS (SELECT 1 FROM probe WHERE probe.body = comp.body)')
      control = cost(session, 'SELECT id FROM control WHERE EXISTS (SELECT 1 FROM probe WHERE probe.body = control.body)')
      assert_equal(3, comp - control, 'correlated outer reference touches body')
    end
  end

  def exact_assignment_cost
    with_database do |session|
      session.execute('CREATE TABLE t (id i32 PRIMARY KEY, body text, n i32)')
      session.execute("INSERT INTO t VALUES (1, 'small', 0)")
      # One page, one row, and the equality predicate. Assignment itself adds
      # no unit; its RHS operators and compression attempts add their own.
      assert_equal(3, cost(session, 'UPDATE t SET n = 1 WHERE id = 1'), 'constant assignment cost')
      assert_equal(4, cost(session, 'UPDATE t SET n = n + 1 WHERE id = 1'), 'arithmetic assignment cost')
      assert_equal(6, cost(session, "UPDATE t SET body = '#{'x' * 600}' WHERE id = 1"), 'assignment with compression cost')
    end
  end

  def run
    tests = %i[scan_decompression external_compressed_scan bounded_scan_and_limit insert_attempts update_attempts
               alter_add_attempts alter_drop_attempts decimal_payload untouched_columns correlated_touch exact_assignment_cost]
    failures = tests.filter_map do |name|
      public_send(name)
      nil
    rescue StandardError => error
      "#{name}: #{error.class}: #{error.message}"
    end
    raise failures.join("\n") unless failures.empty?
    puts "ruby-native compressed costs: #{tests.length}/#{tests.length} host cases passed"
  end
end

RubyCompressedCostTest.run
