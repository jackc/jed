# frozen_string_literal: true

require 'json'
require 'tmpdir'
require_relative '../lib/jed_native'

module RubyStorageForkTest
  def self.assert(value, message) = (raise message unless value)

  def self.inherited_state
    yield
    raise 'inherited handle was accepted'
  rescue JedNative::Error => error
    assert(error.sqlstate == '55006', "unexpected inherited-handle error #{error.sqlstate}")
  end

  def self.run
    Dir.mktmpdir('jed-ruby-fork') do |directory|
      path = File.join(directory, 'db.jed')
      database = JedNative.create(path, locking: :auto)
      database.execute('CREATE TABLE t (id i32 PRIMARY KEY)')
      database.execute('INSERT INTO t VALUES (1)')
      owner_path = File.join("#{path}.lock", 'owner')
      owner_bytes = File.binread(owner_path)
      child_read, parent_write = IO.pipe
      parent_read, child_write = IO.pipe
      pid = fork do
        child_read.close
        child_write.close
        begin
          inherited_state { database.execute('INSERT INTO t VALUES (99)') }
          inherited_state { database.close }
          reopened = JedNative.open(path, locking: :auto)
          storage = reopened.engine.instance_variable_get(:@storage)
          assert(storage.service_role == :client, 'child stole the parent owner lock')
          assert(reopened.query('SELECT id FROM t').rows == [[1]], 'reopened child did not read owner pages')
          reopened.execute('INSERT INTO t VALUES (2)')
          parent_write.puts(JSON.generate(ready: true))
          parent_write.flush
          raise 'parent ended early' unless parent_read.gets
          assert(reopened.query('SELECT id FROM t ORDER BY id').rows == [[1], [2], [3]], 'child did not adopt the parent commit')
          reopened.close
          parent_write.puts(JSON.generate(done: true))
          parent_write.close
          exit! 0
        rescue Exception => error
          parent_write.puts(JSON.generate(error: "#{error.class}: #{error.message}", backtrace: error.backtrace&.first(8))) rescue nil
          parent_write.flush rescue nil
          exit! 1
        end
      end
      parent_read.close
      parent_write.close
      begin
        first = JSON.parse(child_read.gets || '{}')
        assert(first['ready'], "forked child failed: #{first.inspect}")
        assert(File.binread(owner_path) == owner_bytes, 'child changed the parent owner record')
        File.open(File.join("#{path}.lock", 'presence'), File::RDWR) do |probe|
          assert(!probe.flock(File::LOCK_EX | File::LOCK_NB), 'child released the parent presence lock')
        end
        assert(database.query('SELECT id FROM t ORDER BY id').rows == [[1], [2]], 'parent did not adopt child commit')
        database.execute('INSERT INTO t VALUES (3)')
        child_write.puts('continue')
        child_write.close
        final = JSON.parse(child_read.gets || '{}')
        assert(final['done'], "forked child failed: #{final.inspect}")
        _, status = Process.waitpid2(pid)
        pid = nil
        assert(status.success?, 'forked child exited unsuccessfully')
        assert(File.binread(owner_path) == owner_bytes, 'child close removed the parent owner record')
        database.execute('INSERT INTO t VALUES (4)')
        assert(database.query('SELECT count(*) FROM t').rows == [[4]], 'parent failed after child closed')
      ensure
        if pid
          Process.kill('KILL', pid) rescue nil
          Process.waitpid(pid) rescue nil
        end
        [child_read, child_write].each { |io| io.close unless io.closed? }
        database.close
      end
    end
    puts 'ruby-native fork: inherited handles reject, fresh child client preserves parent locks and writes'
  end
end

RubyStorageForkTest.run
