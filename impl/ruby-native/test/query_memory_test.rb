# frozen_string_literal: true

require_relative '../lib/jed_native'

# Host budget changes and encoded pending-write ownership are API invariants;
# SQL operator thresholds remain in the shared resource/query_memory corpus.
module NativeQueryMemoryTests
  module_function

  def rejected(db, bytes, sql)
    db.max_query_memory_bytes = bytes
    db.query(sql)
    raise "expected memory rejection at #{bytes} bytes"
  rescue JedNative::Error => error
    raise unless error.sqlstate == '54P05'
  end

  def run
    JedNative.memory do |db|
      db = db.session
      db.query('CREATE TABLE p (id i32 PRIMARY KEY)')
      db.query('CREATE TABLE c (id i32 PRIMARY KEY, p i32 REFERENCES p(id) ON UPDATE CASCADE)')
      db.query('INSERT INTO p VALUES (1)')
      db.query('INSERT INTO c VALUES (1, 1)')
      rejected(db, 19, 'UPDATE p SET id = 2')
      db.max_query_memory_bytes = 20
      db.query('UPDATE p SET id = 2')
      db.max_query_memory_bytes = 0
      raise 'cascade did not preserve rollback and retry' unless db.query('SELECT p FROM c').rows == [[2]]

      rejected(db, 11, 'ALTER TABLE p ADD COLUMN x i32 DEFAULT 4')
      db.max_query_memory_bytes = 12
      db.query('ALTER TABLE p ADD COLUMN x i32 DEFAULT 4')
      rejected(db, 8, 'CREATE INDEX c_p ON c (p)')
      db.max_query_memory_bytes = 9
      db.query('CREATE INDEX c_p ON c (p)')
      db.max_query_memory_bytes = 0
      raise 'DDL memory error leaked its schema change' unless db.query('SELECT * FROM p').rows == [[2, 4]]
    end
    puts 'ruby-native query memory: host budgets, FK staging, DDL staging, rollback and retry passed'
  end
end

NativeQueryMemoryTests.run
