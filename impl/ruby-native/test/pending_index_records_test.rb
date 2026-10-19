# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

module RubyPendingIndexRecordsTest
  module SizingScope
    def query_memory_stage_changes(table, changes)
      previous = Thread.current[:jed_pending_index_allowed_pages]
      if Thread.current[:jed_pending_index_test]
        Thread.current[:jed_pending_index_allowed_pages] = changes.filter_map do |before, _|
          before.storage_leaf.page if before.is_a?(JedNative::PagedRow)
        end.to_set
      end
      super
    ensure
      Thread.current[:jed_pending_index_allowed_pages] = previous
    end
  end

  module GuardUnchangedPages
    def page(index)
      allowed = Thread.current[:jed_pending_index_allowed_pages]
      raise 'pending-entry sizing read an unchanged index page' if allowed && !allowed.include?(index)
      super
    end
  end
  JedNative::Engine.prepend(SizingScope)
  JedNative::LeafPool.prepend(GuardUnchangedPages)

  def self.run
    database = session = nil
    Dir.mktmpdir('jed-ruby-pending-index') do |directory|
      path = File.join(directory, 'db')
      database = JedNative.create(path, page_size: 512, locking: :exclusive)
      session = database.session
      session.execute('CREATE TABLE t (id i32 PRIMARY KEY, v i32, tags i32[])')
      session.execute('INSERT INTO t SELECT g::i32, (g%17)::i32, ARRAY[(g%7)::i32] FROM generate_series(1,500) g')
      session.execute('CREATE INDEX t_v ON t(v)')
      session.execute('CREATE INDEX t_tags ON t USING gin(tags)')
      session.close
      database.close
      database = JedNative.open(path, cache_bytes: 512, locking: :exclusive)
      session = database.session
      session.execute('BEGIN')
      Thread.current[:jed_pending_index_test] = true
      session.execute('UPDATE t SET v = v + 1, tags = ARRAY[9] WHERE id = 250')
      Thread.current[:jed_pending_index_test] = false
      raise 'pending B-tree key missing' unless session.execute('SELECT id FROM t WHERE v = 13 AND id = 250').rows == [[250]]
      raise 'pending GIN key missing' unless session.execute("SELECT id FROM t WHERE tags @> '{9}'::i32[]").rows == [[250]]
      session.execute('ROLLBACK')
      raise 'rollback changed indexed row' unless session.execute('SELECT v FROM t WHERE id = 250').rows == [[12]]
      session.close
      database.close
    end
    puts 'ruby-native pending indexes: singleton sizing avoids unchanged B-tree/GIN pages and preserves rollback'
  ensure
    Thread.current[:jed_pending_index_test] = false
    Thread.current[:jed_pending_index_allowed_pages] = nil
    session&.close
    database&.close
  end
end

RubyPendingIndexRecordsTest.run
