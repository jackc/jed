# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

module RubyPagedStorageTest
  def self.assert(value, message)
    raise message unless value
  end

  def self.cache_and_pins
    Dir.mktmpdir('jed-ruby-pager') do |directory|
      path = File.join(directory, 'db.jed')
      database = JedNative.create(path, page_size: 512, cache_bytes: 512)
      writer = database.session
      writer.execute('CREATE TABLE t (id i32 PRIMARY KEY, v text)')
      writer.execute("INSERT INTO t SELECT g::i32, 'value-' || g::text FROM generate_series(1, 240) g")
      writer.execute('CREATE INDEX t_v ON t (v DESC)')
      writer.close
      database.close

      database = JedNative.open(path, cache_bytes: 512)
      reader = database.read_session
      table = reader.engine.instance_variable_get(:@tables).fetch('t')
      rows = table[:rows]
      assert(rows.is_a?(JedNative::PagedRows), 'opening a file materialized its rows')
      assert(rows.pool.cached_leaves.zero?, 'opening a file decoded data leaves')
      assert(reader.query('SELECT v FROM t WHERE id = 239').rows == [['value-239']], 'point read failed')
      assert(!rows.instance_variable_defined?(:@offsets), 'point read scanned every leaf to calculate a row ordinal')
      assert(reader.query("SELECT id FROM t WHERE v = 'value-239'").rows == [[239]], 'secondary point read failed')
      assert(reader.query('SELECT id FROM t WHERE id IN (2, 239) ORDER BY id').rows == [[2], [239]], 'point-set cursor failed')
      assert(!rows.instance_variable_defined?(:@offsets), 'bounded index access scanned every leaf')
      expected = reader.query('SELECT id, v FROM t ORDER BY v DESC LIMIT 17').rows
      assert(expected.length == 17, 'ordered secondary cursor lost rows')
      assert(rows.pool.cached_leaves <= 1, 'decoded leaf cache exceeded its capacity')
      assert(rows.pool.image.cached_pages <= 1, 'raw page cache exceeded its capacity')

      writer = database.session
      writer.execute('BEGIN')
      writer.execute("UPDATE t SET v = 'pending' WHERE id = 239")
      pending = writer.engine.instance_variable_get(:@tables).fetch('t')[:rows]
      assert(pending.is_a?(JedNative::PagedRows), 'pending update materialized committed rows')
      changed = pending.instance_variable_get(:@leaves).count { |leaf| leaf.is_a?(JedNative::RecordLeaf) }
      assert(changed.positive? && changed < pending.instance_variable_get(:@leaves).length, 'point update copied every primary leaf')
      assert(writer.query('SELECT v FROM t WHERE id = 239').rows == [['pending']], 'pending tree did not expose its new value')
      assert(reader.query('SELECT v FROM t WHERE id = 239').rows == [['value-239']], 'pending write escaped its transaction')
      writer.execute('ROLLBACK')
      18.times do |generation|
        writer.execute("UPDATE t SET v = 'generation-#{generation}-' || id::text")
      end
      assert(reader.query('SELECT v FROM t WHERE id = 239').rows == [['value-239']], 'a writer reclaimed a pinned reader page')
      assert(reader.query('SELECT id, v FROM t ORDER BY v DESC LIMIT 17').rows == expected, 'a pinned secondary root changed')
      assert(writer.query('SELECT v FROM t WHERE id = 239').rows == [['generation-17-239']], 'writer did not publish')
      reader.close
      4.times { |generation| writer.execute("UPDATE t SET v = 'released-#{generation}-' || id::text") }
      writer.close
      database.close

      database = JedNative.open(path, cache_bytes: 512)
      assert(database.query('SELECT v FROM t WHERE id = 239').rows == [['released-3-239']], 'reclaimed file did not reopen')
      database.close
    end
  end

  def self.auxiliary_reopen
    Dir.mktmpdir('jed-ruby-auxiliary-pager') do |directory|
      path = File.join(directory, 'db.jed')
      database = JedNative.create(path, cache_bytes: 8192)
      writer = database.session
      writer.execute('CREATE TABLE t (id i32 PRIMARY KEY, tags i32[], span int4range)')
      writer.execute("INSERT INTO t SELECT g::i32, ARRAY[(g % 7)::i32], int4range(g::i32, (g+2)::i32) FROM generate_series(1, 220) g")
      writer.execute('CREATE INDEX t_tags ON t USING gin (tags)')
      writer.execute('CREATE INDEX t_span ON t USING gist (span)')
      queries = ["SELECT id FROM t WHERE tags @> '{3}'::i32[] ORDER BY id", "SELECT id FROM t WHERE span && '[103,107)'::int4range ORDER BY id"]
      expected = queries.map { |sql| result = writer.query(sql); [result.rows, result.cost] }
      writer.close
      database.close
      database = JedNative.open(path, cache_bytes: 8192)
      reader = database.session
      table = reader.engine.instance_variable_get(:@tables).fetch('t')
      assert(table[:rows].is_a?(JedNative::PagedRows), 'auxiliary index forced row residency')
      actual = queries.map { |sql| result = reader.query(sql); [result.rows, result.cost] }
      assert(actual == expected, 'auxiliary pages changed values or costs across reopen')
      storage = reader.engine.instance_variable_get(:@storage)
      assert(storage.load[:live_pages].to_a.length == storage.meta[:live], 'lazy auxiliary live-page walk is incomplete')
      reader.execute("INSERT INTO t VALUES (221, '{3}', '[221,223)')")
      reader.close
      database.close
    end
  end

  def self.run
    cache_and_pins
    auxiliary_reopen
    puts 'ruby-native pager: 2 tests passed (bounded caches, pinned roots, lazy auxiliary indexes)'
  end
end

RubyPagedStorageTest.run
