# frozen_string_literal: true
require_relative '../lib/jed_native'

module RubyHostBoundaryParityTest
  module_function
  def error(code)
    yield
    raise "expected #{code}"
  rescue JedNative::Error => exception
    raise "expected #{code}, got #{exception.sqlstate}" unless exception.sqlstate == code
  end
  def run
    JedNative.memory do |db|
      session = db.session
      session.execute('CREATE TABLE t (id i32 PRIMARY KEY)')
      %w[COMMIT ROLLBACK].each do |command|
        error('25P02') do
          session.update do |tx|
            tx.execute('INSERT INTO t VALUES (1)')
            error('2D000') { tx.execute(command) }
            error('25P02') { tx.execute('SELECT 1') }
          end
        end
        raise 'failed closure published writes' unless session.query('SELECT * FROM t').rows.empty?
      end
      session.update do |tx|
        error('2D000') { tx.commit }
        error('2D000') { tx.rollback }
        error('55006') { db.close }
        error('55006') { db.attach('blocked') }
        tx.execute('INSERT INTO t VALUES (2)')
      end
      raise 'refused host methods aborted closure' unless session.query('SELECT * FROM t').rows == [[2]]
      ["bad\0text", "bad\xff".b].each do |text|
        error('22021') { session.execute(text) }
        error('22021') { session.execute('SELECT $1::text', text) }
        error('22021') { session.query('SELECT $1::text[]', [text]) }
      end
      session.execute('CREATE TYPE r AS (s text)')
      error('22021') { session.execute('SELECT $1::r', ["nested\0text"]) }
      bytes = "\x00\xff".b
      raise 'binary parameter rejected' unless session.query('SELECT $1::bytea', bytes).rows == [[JedNative::Bytea.new(bytes)]]
      session.close
    end
    puts 'ruby-native host boundaries: closure ownership, failed blocks, close/attach refusal and nested text validation passed'
  end
end
RubyHostBoundaryParityTest.run
