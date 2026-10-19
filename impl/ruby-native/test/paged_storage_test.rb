# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

module RubyPagedStorageTest
  def self.assert(value, message)
    raise message unless value
  end

  class LeafFixtureCodec
    attr_reader :reads
    attr_accessor :on_parse
    def initialize = (@reads = 0)
    def read_page(*)
      @reads += 1
      { type: JedNative::FileFormat::PAGE_LEAF, payload: ''.b, item_count: 1 }
    end
    def parse_pax_leaf(*)
      @on_parse&.call
      [["key".b], []]
    end
  end

  def self.decoded_schema_identity
    cache = JedNative::DecodedLeafCache.new(2)
    image = Object.new
    image.define_singleton_method(:decoded_leaf_cache) { cache }
    codec = LeafFixtureCodec.new
    first = JedNative::LeafPool.new(codec, image, 512, 2)
    second = JedNative::LeafPool.new(codec, image, 512, 2)
    schemas = 3.times.map do
      columns = [{ name: 'id', type: 'i64' }]
      columns.define_singleton_method(:hash) { raise 'leaf access hashed the column schema' }
      columns
    end
    a = first.leaf(3, schemas[0])
    b = first.leaf(3, schemas[1])
    assert(!a.equal?(b), 'different schema identities shared a decoded entry')
    assert(a[:schema].equal?(schemas[0]), 'cache key does not retain its schema identity')
    assert(second.leaf(3, schemas[0]).equal?(a) && codec.reads == 2, 'shared pool lost schema identity reuse')
    first.leaf(4, schemas[2])
    assert(first.leaf(3, schemas[0]).equal?(a), 'recent schema entry was evicted')
    assert(!first.leaf(3, schemas[1]).equal?(b), 'least-recent schema entry was retained')
    assert(cache.size == 2 && codec.reads == 4, 'schema identities escaped the bounded LRU')
    cache.invalidate([3])
    assert(cache.size.zero?, 'page invalidation missed a schema variant')
    codec.on_parse = -> { cache.invalidate([3]) }
    first.leaf(3, schemas[0])
    assert(cache.size.zero?, 'in-flight decode escaped generation invalidation')
  end

  def self.direct_leaf_values
    codec = JedNative::FileFormat.new
    values = {
      'i16' => [-(1 << 15), -1, 0, (1 << 15) - 1],
      'i32' => [-(1 << 31), -1, 0, (1 << 31) - 1],
      'i64' => [-(1 << 63), -1, 0, (1 << 63) - 1],
      'date' => [-(1 << 31), 17, (1 << 31) - 1],
      'timestamp' => [-(1 << 63), 42, (1 << 63) - 1],
      'timestamptz' => [-(1 << 63), -42, (1 << 63) - 1],
      'boolean' => [false, true],
      'uuid' => ['12345678-90ab-cdef-1234-567890abcdef'],
      'interval' => [[-12, 34, -(1 << 63)], [12, -34, (1 << 63) - 1]],
      'f32' => [-Float::INFINITY, -0.0, 1.5, Float::INFINITY, Float::NAN],
      'f64' => [-Float::INFINITY, -0.0, 1.5, Float::INFINITY, Float::NAN]
    }
    values.each do |type, entries|
      width = codec.fixed_width(type)
      bytes = entries.map { |value| codec.encode_value(type, value) }
      # Put NULL first so every tested value has a nonzero slot offset.
      bitmap = "\x80".b.ljust((entries.length + 8) / 8, "\x00".b)
      region = { width: width, bitmap: bitmap, body: "\x00".b * width + bytes.map { |entry| entry.byteslice(1, width) }.join }
      assert(codec.leaf_value(region, type, 0, nil).nil?, "#{type} NULL bitmap was ignored")
      entries.each_index do |index|
        value = codec.leaf_value(region, type, index + 1, nil)
        assert(codec.encode_value(type, value) == bytes[index], "#{type} fixed PAX slot changed bytes at #{index}")
      end
    end
    { 'text' => ['', 'snowman ☃'], 'json' => ['{"x":"☃"}'], 'bytea' => ["\x00\xff".b] }.each do |type, entries|
      bytes = entries.map { |value| codec.encode_value(type, value) }
      total = 0
      region = { ends: [0] + bytes.map { |entry| total += entry.bytesize }, body: bytes.join }
      assert(codec.leaf_value(region, type, 0, nil).nil?, "#{type} NULL span was ignored")
      entries.each_with_index do |expected, index|
        value = codec.leaf_value(region, type, index + 1, nil)
        assert(value == expected, "#{type} inline value changed")
        assert(value.encoding == (type == 'bytea' ? Encoding::ASCII_8BIT : Encoding::UTF_8), "#{type} inline encoding changed")
        value.replace('changed')
        assert(codec.leaf_value(region, type, index + 1, nil) == expected, 'caller mutation changed cached PAX bytes')
      end
    end
    malformed = [
      [{ width: 1, bitmap: "\x00".b, body: "\x02".b }, 'boolean'],
      [{ width: 8, bitmap: "\x00".b, body: "\x00".b * 7 }, 'i64'],
      # The declared length crosses into a following value even though the
      # overall region contains enough bytes. Only the first span is legal.
      [{ ends: [4, 8], body: "\x00\x00\x02a\x00\x00\x01b".b }, 'text'],
      [{ ends: [4], body: "\x00\x00\x01\xff".b }, 'text']
    ]
    malformed.each do |region, type|
      begin
        codec.leaf_value(region, type, 0, nil)
      rescue StandardError
        next
      end
      raise "accepted malformed #{type} PAX value"
    end
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

  # Disk corpus records reopen after autocommit. This host sequence deliberately
  # keeps the original C-labelled column descriptors across CREATE and INSERT.
  def self.c_collated_constraint_keys
    Dir.mktmpdir('jed-ruby-c-keys') do |directory|
      path = File.join(directory, 'db')
      database = JedNative.create(path, page_size: 512, locking: :exclusive)
      session = database.session
      expected = {}
      { 'text_keys' => 'text', 'array_keys' => 'text[]' }.each do |name, type|
        session.execute("CREATE TABLE #{name}(id #{type} COLLATE \"C\" PRIMARY KEY, v #{type} COLLATE \"C\" UNIQUE)")
        values = type == 'text' ? %w[a m z] : [%w[a], %w[a m], %w[z]]
        insert = session.prepare("INSERT INTO #{name} VALUES($1, $2)")
        session.execute_prepared(insert, values[0], values[1])
        session.execute_prepared(insert, values[1], values[2])
        [[values[0], values[0]], [values[2], values[1]]].each do |params|
          begin
            session.execute_prepared(insert, *params)
            raise "#{type} duplicate constraint key was accepted"
          rescue JedNative::Error => error
            assert(error.sqlstate == '23505', "#{type} duplicate returned #{error.sqlstate}")
          end
        end
        table = session.engine.tables.fetch(name)
        codec = table.fetch(:native_primary_codec)
        keys = values.first(2).map { |value| codec.key_body(type, JedNative::Types.storage_value(type, value)) }
        assert(table[:rows].keys.to_a == keys, "#{type} C constraint key differs from canonical stored key bytes")
        expected[name] = [[values[0], values[1]], [values[1], values[2]]]
        assert(session.execute("SELECT * FROM #{name} ORDER BY id").rows == expected[name], "#{type} live rows changed after duplicate rejection")
      end
      session.close
      database.close
      database = JedNative.open(path, locking: :exclusive)
      session = database.session
      expected.each do |name, values|
        assert(session.execute("SELECT * FROM #{name} ORDER BY id").rows == values, 'C constraint keys changed across reopen')
      end
    ensure
      session&.close
      database&.close
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

  def self.shared_decoded_budget
    Dir.mktmpdir('jed-ruby-shared-leaves') do |directory|
      database = JedNative.create(File.join(directory, 'db'), page_size: 512, cache_bytes: 1024)
      writer = database.session
      %w[a b c].each do |name|
        writer.execute("CREATE TABLE #{name} (id i32 PRIMARY KEY, v text)")
        writer.execute("INSERT INTO #{name} SELECT g::i32, '#{name}-' || g::text FROM generate_series(1, 40) g")
      end
      readers, pools = [], []
      6.times do |generation|
        readers << database.read_session
        %w[a b c].each do |name|
          reader = readers.last
          assert(reader.query("SELECT v FROM #{name} WHERE id = 39").rows.length == 1, 'shared-cache read failed')
          pools << reader.engine.tables.fetch(name)[:rows].pool
        end
        writer.execute("UPDATE #{%w[a b c][generation % 3]} SET v = 'generation-#{generation}' WHERE id = 39")
      end
      assert(pools.map { |pool| pool.decoded_cache.object_id }.uniq.length == 1, 'generations retained independent decoded budgets')
      assert(pools.all? { |pool| pool.cached_leaves <= 2 }, 'shared decoded cache exceeded its device budget')
      assert(readers.first.query('SELECT v FROM a WHERE id = 39').rows == [['a-39']], 'shared cache changed a pinned old page')
      readers.each(&:close)
      8.times { |index| writer.execute("UPDATE a SET v = 'reuse-#{index}'") }
      assert(writer.query('SELECT v FROM a WHERE id = 39').rows == [['reuse-7']], 'reused page retained stale decoded bytes')
      writer.close
      database.close
    end
  end

  def self.index_build_keeps_primary_pages
    Dir.mktmpdir('jed-ruby-index-pager') do |directory|
      path = File.join(directory, 'db')
      database = JedNative.create(path, page_size: 512, cache_bytes: 1024)
      writer = database.session
      writer.execute('CREATE TABLE t (id i32 PRIMARY KEY, v i32, tags i32[], span int4range)')
      writer.execute('INSERT INTO t SELECT g::i32, (g % 7)::i32, ARRAY[(g % 7)::i32], int4range(g::i32, (g+2)::i32) FROM generate_series(1, 160) g')
      original = File.binread(path)
      writer.execute('BEGIN')
      rows = writer.engine.tables.fetch('t')[:rows]
      root = rows.tree
      statements = ['CREATE INDEX t_v ON t (v)', 'CREATE INDEX t_tags ON t USING gin (tags)',
                    'CREATE INDEX t_span ON t USING gist (span)']
      statements.each do |sql|
        writer.execute(sql)
        table = writer.engine.tables.fetch('t')
        assert(table[:rows].equal?(rows), 'CREATE INDEX copied the primary row inventory')
        assert(table[:native_primary_tree].equal?(root), 'CREATE INDEX rebuilt the primary tree')
      end
      indexes = writer.engine.tables.fetch('t')[:indexes]
      writer.execute('ANALYZE t (v, tags)')
      analyzed = writer.engine.tables.fetch('t')
      assert(analyzed[:rows].equal?(rows), 'ANALYZE copied the primary row inventory')
      assert(analyzed[:native_primary_tree].equal?(root), 'ANALYZE rebuilt the primary tree')
      assert(analyzed[:indexes].equal?(indexes), 'ANALYZE rebuilt index storage profiles')
      assert(analyzed[:storage_statistics].length == 2, 'ANALYZE lost selected column facts')
      writer.execute("INSERT INTO t VALUES (161, 3, '{3}', '[161,163)')")
      assert(writer.query('SELECT id FROM t WHERE v = 3 ORDER BY id').rows.last == [161], 'pending btree did not accept a write')
      assert(writer.query("SELECT id FROM t WHERE tags @> '{3}'::i32[] ORDER BY id").rows.last == [161], 'pending GIN did not accept a write')
      assert(writer.query("SELECT id FROM t WHERE span && '[161,162)'::int4range ORDER BY id").rows.last == [161], 'pending GiST did not accept a write')
      writer.execute('SAVEPOINT keep_indexes')
      writer.execute('DROP INDEX t_v')
      writer.execute('ROLLBACK TO keep_indexes')
      assert(writer.engine.tables.fetch('t')[:native_secondary_metadata].key?('t_v'), 'index rollback lost its pending tree')
      writer.execute('ROLLBACK')
      assert(File.binread(path) == original, 'rolled-back index builds changed committed bytes')
      assert(writer.engine.tables.fetch('t')[:indexes].empty?, 'index builds escaped rollback')
      page = writer.engine.tables.fetch('t')[:native_primary_tree][:page]
      writer.execute(statements.first)
      assert(writer.engine.tables.fetch('t')[:native_primary_tree][:page] == page, 'index commit rewrote the primary root')
      writer.close
      database.close
    end
  end

  def self.run
    decoded_schema_identity
    direct_leaf_values
    c_collated_constraint_keys
    cache_and_pins
    auxiliary_reopen
    shared_decoded_budget
    index_build_keeps_primary_pages
    puts 'ruby-native pager: 7 tests passed (PAX codecs, C constraint keys, bounded schema identities, shared caches, pinned roots, streamed index builds and statistics)'
  end
end

RubyPagedStorageTest.run
