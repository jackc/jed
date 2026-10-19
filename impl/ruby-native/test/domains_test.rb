# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

# Attachment creation/detach, filesystem durability, and host statement reuse
# are outside the SQL transcript's vocabulary.
module NativeDomainTests
  module_function

  def assert(condition, message)
    raise message unless condition
  end

  def error(code)
    yield
    raise "expected #{code}, got success"
  rescue JedNative::Error => exception
    assert(exception.sqlstate == code, "expected #{code}, got #{exception.sqlstate}")
  end

  def run
    JedNative.memory do |db|
      db = db.session
      db.attach('Refs')
      error('42710') { db.attach('Refs') }
      error('42710') { db.attach('main') }
      error('42710') { db.attach('temp') }
      error('42P01') { db.query('SELECT * FROM refs.t') }
      db.execute('CREATE TABLE "Refs".t (id i32 PRIMARY KEY)')
      prepared = db.prepare('SELECT id FROM "Refs".t ORDER BY id')
      db.execute('BEGIN')
      db.execute('INSERT INTO "Refs".t VALUES (1)')
      db.execute('SAVEPOINT s')
      db.execute('INSERT INTO "Refs".t VALUES (2)')
      error('55006') { db.detach('Refs') }
      db.execute('ROLLBACK TO s')
      assert(db.query_prepared(prepared).rows == [[1]], 'attachment savepoint restores its rows')
      db.execute('ROLLBACK')
      assert(db.query_prepared(prepared).rows.empty?, 'attachment transaction rollback restores its rows')
      db.detach('Refs')
      error('42P01') { db.query_prepared(prepared) }
      error('42704') { db.detach('Refs') }
      db.attach('Refs')
      db.execute('CREATE TABLE "Refs".t (id text)')
      db.execute("INSERT INTO \"Refs\".t VALUES ('new')")
      assert(db.query_prepared(prepared).rows == [['new']], 'reattach binds a prepared statement to the new catalog')
      db.attach('ro', :memory, :read_only)
      error('25006') { db.execute('CREATE TABLE ro.t (id i32)') }
    end

    Dir.mktmpdir('jed-ruby-domains') do |directory|
      main_path = File.join(directory, 'main.jed')
      ref_path = File.join(directory, 'ref.jed')
      JedNative.create(ref_path) do |db|
        db = db.session
        db.execute('CREATE TABLE t (id i32 PRIMARY KEY, value text)')
        db.execute("INSERT INTO t VALUES (1, 'original')")
      end
      JedNative.create(main_path) do |db|
        db = db.session
        db.execute('CREATE TABLE t (id i32 PRIMARY KEY)')
        db.attach('ref', ref_path)
        original_main = File.binread(main_path)
        db.execute("UPDATE ref.t SET value = 'updated' WHERE id = 1")
        assert(File.binread(main_path) == original_main, 'attachment-only commit must not rewrite main')
        db.execute('BEGIN')
        db.execute('INSERT INTO t VALUES (1)')
        db.execute("UPDATE ref.t SET value = 'forbidden' WHERE id = 1")
        error('0A000') { db.execute('COMMIT') }
        db.execute('ROLLBACK')
        assert(db.query('SELECT * FROM t').rows.empty?, 'second durable writer rejection rolls back main')
        db.execute('BEGIN')
        db.execute('SAVEPOINT s')
        db.execute('INSERT INTO t VALUES (2)')
        db.execute('ROLLBACK TO s')
        db.execute("UPDATE ref.t SET value = 'savepoint' WHERE id = 1")
        db.execute('COMMIT')
        db.detach('ref')
        db.attach('ref', ref_path, :read_only)
        assert(db.query('SELECT value FROM ref.t').rows == [['savepoint']], 'attachment persisted its own catalog')
        error('25006') { db.execute("UPDATE ref.t SET value = 'forbidden'") }
        error('25006') { db.execute('DROP TABLE ref.t') }
        db.execute('INSERT INTO t VALUES (3)')
      end
      JedNative.open(ref_path) do |db|
        db = db.session
        assert(db.query('SELECT id, value FROM t').rows == [[1, 'savepoint']], 'standalone reopen sees attachment commits')
      end
      JedNative.open(main_path) do |db|
        db = db.session
        assert(db.query('SELECT * FROM t').rows == [[3]], 'main commits remain independent')
        error('42P01') { db.query('SELECT * FROM ref.t') }
      end
    end
    puts 'ruby-native domains: host attachment lifecycle, read-only checks, savepoints, isolated durability passed'
  end
end

NativeDomainTests.run
