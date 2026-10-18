# frozen_string_literal: true

# Explicit-only byte-format and host tests. No gems, foreign core, or native
# extension is loaded by these tests.
require 'tmpdir'
require 'fileutils'
require_relative '../lib/jed_native'
require_relative '../lib/jed_native/storage'

module RubyStorageTest
  FIXTURES = File.expand_path('../../../spec/fileformat/fixtures', __dir__)
  REJECTED = %w[cow_invalid_free_list.jed malformed_decimal_group.jed malformed_fk_type.jed
                malformed_interior_cycle.jed malformed_leaf_directory.jed malformed_missing_type.jed
                malformed_overflow_cycle.jed malformed_row_count.jed malformed_wrong_tree_leaf.jed].freeze

  def self.assert(value, message = 'assertion failed')
    raise message unless value
  end

  def self.error(code)
    yield
  rescue JedNative::Error => exception
    assert(exception.sqlstate == code, "expected #{code}, got #{exception.sqlstate}: #{exception.message}")
  else
    raise "expected #{code}"
  end

  def self.snapshot(rows)
    codec = JedNative::FileFormat.new
    { tables: [{ name: 't', columns: [codec.col('id', 'i32', pk: true), codec.col('v', 'text')], rows: rows }] }
  end

  def self.golden_decode
    codec = JedNative::FileFormat.new
    accepted = 0
    Dir[File.join(FIXTURES, '*.jed')].sort.each do |path|
      name = File.basename(path)
      begin
        codec.decode_image(File.binread(path))
        raise "accepted corrupt #{name}" if REJECTED.include?(name)
        accepted += 1
      rescue StandardError => exception
        raise "#{name}: #{exception.message}" unless REJECTED.include?(name)
      end
    end
    assert(accepted == 97, "expected 97 readable fixtures, got #{accepted}")
  end

  def self.golden_encode
    codec = JedNative::FileFormat.new
    count = 0
    Dir[File.join(FIXTURES, '*_table.jed')].sort.each do |path|
      next if File.basename(path).match?(/collation|expr_index|partial_index|timezone_index|hostfunc_index|exclude_followon/)

      expected = File.binread(path)
      state = codec.decode_image(expected)
      actual = codec.build_image(state[:types], state[:sequences], state[:tables], 256,
                                 state[:collations], views: state[:views])
      assert(expected == actual, "encoded bytes differ from #{File.basename(path)}")
      count += 1
    end
    assert(count == 65, "expected 65 byte comparisons, got #{count}")
  end

  def self.durable_recovery
    Dir.mktmpdir('jed-ruby-storage') do |dir|
      path = File.join(dir, 'state.jed')
      store = JedNative::Storage.new(path: path, create: true, page_size: 256)
      store.commit(snapshot([[1, 'old']]))
      old_meta = store.meta.dup
      rows = 250.times.map { |index| [index, "payload-#{index}"] }
      store.commit(snapshot(rows))
      newest_meta = store.meta.dup
      store.close
      bytes = File.binread(path)
      offset = newest_meta[:slot] * 256
      assert(bytes.byteslice(offset + 44, 4).unpack1('N').positive?, 'overflow manifest was not exercised')
      store = JedNative::Storage.new(path: path)
      assert(store.load[:tables].first[:rows] == rows)
      store.close
      # The new root is checksummed in the manifest; damaging it must fall back
      # to the preceding complete generation without replaying any body pages.
      File.open(path, 'r+b') do |file|
        where = newest_meta[:root_page] * 256 + 16
        byte = file.pread(1, where).getbyte(0)
        file.pwrite([byte ^ 1].pack('C'), where)
        file.fsync
      end
      store = JedNative::Storage.new(path: path)
      assert(store.meta[:txid] == old_meta[:txid], 'did not recover the previous generation')
      assert(store.load[:tables].first[:rows] == [[1, 'old']])
      store.commit(snapshot([[2, 'after recovery']]))
      store.close
      store = JedNative::Storage.new(path: path)
      assert(store.load[:tables].first[:rows] == [[2, 'after recovery']])
      store.close
    end
  end

  def self.locking
    Dir.mktmpdir('jed-ruby-lock') do |dir|
      path = File.join(dir, 'locked.jed')
      store = JedNative::Storage.new(path: path, create: true)
      error('55006') { JedNative::Storage.new(path: path, file_lock_timeout_ms: 0) }
      # A separate OS descriptor must see the same protocol-v2 ownership lock.
      File.open("#{path}.lock/presence", 'r+b') do |presence|
        assert(!presence.flock(File::LOCK_EX | File::LOCK_NB), 'presence lock was not held')
      end
      store.close
      File.link(path, File.join(dir, 'alias.jed'))
      error('55006') { JedNative::Storage.new(path: path) }
    end
  end

  def self.storage_limit_and_serialization
    Dir.mktmpdir('jed-ruby-limit') do |dir|
      path = File.join(dir, 'bounded.jed')
      store = JedNative::Storage.new(path: path, create: true, page_size: 256, max_storage_bytes: 256)
      error('54P06') { store.commit(snapshot([[1, 'more than just a catalog page']])) }
      assert(store.meta[:txid] == 1)
      error('54000') { store.commit(tables: [{ name: 'bad', columns: [{ name: 'x', type: 'unavailable' }], rows: [[1]] }]) }
      assert(store.load[:tables].empty?, 'serialization error poisoned or altered the committed snapshot')
      store.close
    end
  end

  def self.io_failure
    Dir.mktmpdir('jed-ruby-failure') do |dir|
      path = File.join(dir, 'failed.jed')
      store = JedNative::Storage.new(path: path, create: true, page_size: 256)
      file = store.instance_variable_get(:@file)
      def file.pwrite(*) = raise(IOError, 'injected write failure')
      error('58030') { store.commit(snapshot([[1, 'new']])) }
      error('58030') { store.load }
      error('58030') { store.close }
      reopened = JedNative::Storage.new(path: path)
      assert(reopened.load[:tables].empty?)
      reopened.close
    end
  end

  def self.sql_reopen
    Dir.mktmpdir('jed-ruby-sql') do |dir|
      path = File.join(dir, 'sql.jed')
      db = JedNative.create(path, page_size: 256)
      db.execute("create table t (id i32 primary key, v text default 'hello', n decimal(9,2))")
      db.execute('insert into t (id,n) values (1,12.50)')
      db.execute('create index t_v on t (v desc)')
      db.execute('create view visible as select id,v from t')
      txid = db.engine.instance_variable_get(:@storage).meta[:txid]
      db.execute('begin')
      db.execute('commit')
      assert(db.engine.instance_variable_get(:@storage).meta[:txid] == txid, 'empty transaction wrote main')
      db.close
      db = JedNative.open(path)
      assert(db.query('select id,v,n from t').rows.map { |row| row.map(&:to_s) } == [['1', 'hello', '12.50']])
      assert(db.query('select * from visible').rows == [[1, 'hello']])
      db.execute('insert into t (id) values (2)')
      db.close
      db = JedNative.open(path)
      assert(db.query('select id,v from t order by id').rows == [[1, 'hello'], [2, 'hello']])
      db.close
    end
  end

  def self.view_catalog_reopen
    Dir.mktmpdir('jed-ruby-view') do |dir|
      path = File.join(dir, 'views.jed')
      db = JedNative.create(path, page_size: 256)
      db.execute('create table source (id i32 primary key, value i32)')
      db.execute('create view visible with (security_barrier=true) as select id,value from source where value > 0 with local check option')
      db.execute('alter view visible alter column value set default 7')
      db.close
      db = JedNative.open(path)
      row = db.query('select check_option,security_barrier from jed_views').rows.first
      assert(row == ['LOCAL', true], "view options were not restored: #{row.inspect}")
      db.execute('insert into visible (id) values (1)')
      error('44000') { db.execute('insert into visible values (2,-1)') }
      db.execute('alter view visible rename to public_rows')
      db.execute('alter view public_rows alter column value set default 9')
      db.execute('alter table source rename to backing')
      definition = db.query('select definition from jed_views').rows.first.first
      assert(definition.include?('backing'), 'renamed view dependency retained old definition')
      db.close
      db = JedNative.open(path)
      db.execute('insert into public_rows (id) values (2)')
      assert(db.query('select id,value from backing order by id').rows == [[1, 7], [2, 9]])
      error('2BP01') { db.execute('drop table backing') }
      db.close
    end
  end

  def self.view_query_reopen
    Dir.mktmpdir('jed-ruby-view-query') do |dir|
      path = File.join(dir, 'query.jed')
      db = JedNative.create(path, page_size: 256)
      db.execute('create table t (id i32 primary key, bucket i32)')
      db.execute('insert into t values (1,1),(2,1),(3,2)')
      definitions = {
        'rank_rows' => 'select id,row_number() over w as rank from t window w as (order by id desc)',
        'union_rows' => '(select id from t order by id limit 1) union all (select id from t order by id desc limit 1)',
        'any_rows' => 'select id from t where id = any(array[1,3])'
      }
      definitions.each { |name, query| db.execute("create view #{name} as #{query}") }
      expected = definitions.to_h { |name, _| [name, db.query("select * from #{name} order by 1").rows] }
      db.close
      db = JedNative.open(path)
      definitions.each do |name, _|
        actual = db.query("select * from #{name} order by 1").rows
        assert(actual == expected[name], "stored view #{name} changed query semantics: #{actual.inspect}")
      end
      db.close
    end
  end

  def self.exclusion_reopen
    Dir.mktmpdir('jed-ruby-exclusion') do |dir|
      path = File.join(dir, 'exclude.jed')
      db = JedNative.create(path, page_size: 256)
      db.execute('create table booking (id i32 primary key, room i32, span i32range, live boolean, exclude using gist (room with =, span with &&) where (live))')
      db.execute("insert into booking values (1,2,'[1,4)'::i32range,true)")
      db.close
      db = JedNative.open(path)
      error('23P01') { db.execute("insert into booking values (2,2,'[3,5)'::i32range,true)") }
      db.execute("insert into booking values (2,2,'[3,5)'::i32range,false)")
      db.close
      db = JedNative.open(path)
      assert(db.query('select id from booking order by id').rows == [[1], [2]])
      db.close
    end
  end

  def self.statistics_reopen
    Dir.mktmpdir('jed-ruby-statistics') do |dir|
      path = File.join(dir, 'stats.jed')
      db = JedNative.create(path, page_size: 256)
      db.execute('create table t (id i32 primary key, v text, tags i32[])')
      db.execute("insert into t values (1,'a',array[1,1,2]),(2,'a',array[2]),(3,null,null)")
      db.execute('create index t_v on t (v desc nulls last)')
      db.execute('analyze t')
      expected = [['id', 0, 3, 3, 4, 3, 0, nil], ['tags', 1, nil, 3, 0, 0, 0, 2], ['v', 1, 1, 3, 3, 1, 0, nil]]
      facts = db.query('select column_name,null_count,distinct_count,sample_rows,average_width,mcv_count,histogram_count,term_count from jed_statistics order by column_name').rows
      assert(facts[0] == expected[0] && facts[2] == expected[2], "scalar statistics changed: #{facts.inspect}")
      assert(facts[1].values_at(0,1,2,3,5,6,7) == expected[1].values_at(0,1,2,3,5,6,7), 'array term statistics changed')
      db.execute('alter table t add column more i32')
      assert(db.query('select is_stale from jed_statistics').rows.all? { |row| row == [false] }, 'ADD COLUMN made statistics stale')
      db.execute('update t set v=v where id=1')
      db.execute('analyze t (v)')
      assert(db.query('select column_name,is_stale from jed_statistics order by column_name').rows == [['id', true], ['tags', true], ['v', false]])
      txid = db.engine.instance_variable_get(:@storage).meta[:txid]
      db.execute('reindex index t_v')
      assert(db.engine.instance_variable_get(:@storage).meta[:txid] > txid, 'REINDEX did not commit the rebuilt index')
      db.close
      db = JedNative.open(path)
      assert(db.query('select column_name,is_stale from jed_statistics order by column_name').rows == [['id', true], ['tags', true], ['v', false]])
      db.close
    end
  end

  def self.run
    tests = %i[golden_decode golden_encode durable_recovery locking storage_limit_and_serialization io_failure sql_reopen view_query_reopen view_catalog_reopen exclusion_reopen statistics_reopen temporal_containers_reopen collation_upgrade incremental_reclamation]
    tests.each do |name|
      public_send(name)
      puts "ruby-native storage: #{name} passed"
    end
    puts "ruby-native storage: #{tests.length} tests passed (106 shared images, 65 byte-exact encodes)"
  end

  def self.temporal_containers_reopen
    Dir.mktmpdir('ruby-native-temporal') do |directory|
      path = File.join(directory, 'temporal.jed')
      db = JedNative.create(path)
      db.execute('create type stamp as (at timestamptz, code i32)')
      db.execute('create table t (id i32 primary key, dates date[], stamps stamp[], span tstzrange)')
      db.execute(<<~SQL)
        insert into t values
        (1, array[date '-infinity', date '2000-01-01', date 'infinity'],
         array[row(timestamptz '-infinity', 1)::stamp, row(timestamptz '2024-01-15 12:00:00+00', 2)::stamp, row(timestamptz 'infinity', 3)::stamp],
         '[-infinity,infinity]'::tstzrange)
      SQL
      expected = db.query('select dates::text, stamps::text, span::text from t').rows
      db.close
      db = JedNative.open(path)
      assert(db.query('select dates::text, stamps::text, span::text from t').rows == expected, 'temporal container storage round trip changed values')
      db.execute('insert into t select 2, dates, stamps, span from t')
      db.close
      db = JedNative.open(path)
      assert(db.query('select dates::text, stamps::text, span::text from t order by id').rows == expected * 2)
      db.close
    end
  end

  def self.collation_upgrade
    bundle = File.expand_path('../../../spec/collation/fixtures/unicode.jucd', __dir__)
    environment = JedNative::Environment.new.load_unicode_data(File.binread(bundle))
    bytes = File.binread(File.join(FIXTURES, 'collation_skew_corrupt.jed'))
    db = JedNative.from_image(bytes, environment: environment)
    db.execute('begin read only')
    error('25006') { db.upgrade_collations }
    db.execute('rollback')
    db.execute('begin')
    assert(db.upgrade_collations == 1)
    db.execute('rollback')
    error('XX002') { db.execute("insert into t values (5, 'c')") }
    db.execute('begin')
    error('42P01') { db.query('select * from missing') }
    error('25P02') { db.upgrade_collations }
    db.execute('rollback')
    assert(db.upgrade_collations == 1)
    assert(db.upgrade_collations == 0)
    assert(db.query("select id from t where name = 'Z'").cost == 5)
    db.close

    Dir.mktmpdir('ruby-native-collation') do |directory|
      path = File.join(directory, 'collation.jed')
      File.binwrite(path, bytes)
      db = JedNative.open(path, environment: environment, read_only: true)
      error('25006') { db.upgrade_collations }
      db.close
      db = JedNative.open(path, environment: environment)
      assert(db.upgrade_collations == 1)
      db.close
      db = JedNative.open(path, environment: environment)
      assert(db.upgrade_collations == 0)
      db.execute("insert into t values (5, 'c')")
      assert(db.query('select id from t order by name').rows == [[1], [3], [4], [5], [2]])
      db.close
    end
  end

  def self.incremental_reclamation
    Dir.mktmpdir('ruby-native-incremental') do |directory|
      path = File.join(directory, 'tree.jed')
      db = JedNative.create(path, page_size: 256)
      db.execute('create table t (id i32 primary key, v text)')
      db.transaction do
        120.times { |id| db.execute('insert into t values ($1, $2)', id, "value-#{id}" * 2) }
      end
      store = db.engine.instance_variable_get(:@storage)
      db.execute("update t set v = 'edited' where id = 60")
      assert(store.meta[:protected].length < store.meta[:live] / 2, 'point update rewrote the whole tree')
      160.times { |id| db.execute('update t set v = $1 where id = 60', "edit-#{id}") }
      assert(store.meta[:page_count] < 3 * store.meta[:live] + 16, 'churn did not reclaim obsolete pages')
      state = JedNative::FileFormat.new.decode_image(File.binread(path))
      assert((state[:free_pages] & state[:meta][:protected]).empty?, 'manifest dependencies became reusable')
      db.execute('delete from t where id >= 20 and id < 100')
      db.close
      db = JedNative.open(path)
      assert(db.query('select count(*) from t').rows == [[40]])
      db.execute("insert into t values (60, 'restored')")
      assert(db.query('select v from t where id = 60').rows == [['restored']])
      db.close
    end
  end
end

RubyStorageTest.run
