# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

module NativeStorageLimitsTest
  module_function

  def error(code)
    yield
    raise "expected #{code}"
  rescue JedNative::Error => failure
    raise unless failure.sqlstate == code
  end

  def check(database)
    first, second = database.session, database.session
    first.execute('CREATE TABLE t (id i32 PRIMARY KEY)')
    database.max_storage_bytes = 1
    error('54P06') { second.execute('INSERT INTO t VALUES (1)') }
    raise 'failed commit leaked a row' unless first.query('SELECT * FROM t').rows.empty?
    first.max_storage_bytes = 0
    second.execute('INSERT INTO t VALUES (1)')
    raise 'session setter did not update the database' unless database.max_storage_bytes.zero?
    error('22023') { database.max_storage_bytes = '1' }
    second.close
    error('55000') { second.max_storage_bytes = 1 }
    first.close
    database.close
    error('55000') { database.max_storage_bytes = 1 }
  end

  def run
    check(JedNative.memory)
    Dir.mktmpdir('jed-ruby-limit-api-') do |directory|
      check(JedNative.create(File.join(directory, 'db.jed')))
    end
    puts 'ruby-native storage limit API: database/session setters, rollback, and closed handles passed'
  end
end

NativeStorageLimitsTest.run
