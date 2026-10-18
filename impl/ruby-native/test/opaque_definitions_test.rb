# frozen_string_literal: true

require_relative '../lib/jed_native'

# A newer writer's catalog text cannot be constructed through this parser's SQL
# API. Check the host image seam and the unmetered rejection contract directly.
module NativeOpaqueDefinitionTests
  module_function

  def assert(value, message)
    raise message unless value
  end

  def reject_without_work(db, sql)
    lifetime = db.engine.lifetime_cost
    begin
      db.execute(sql)
      raise 'expected XX003'
    rescue JedNative::Error => error
      assert(error.sqlstate == 'XX003', "expected XX003, got #{error.sqlstate}")
      assert(db.engine.cost.zero?, 'opaque definition rejection charged work')
      assert(db.engine.lifetime_cost == lifetime, 'opaque definition rejection changed lifetime work')
    end
  end

  def run
    fixture = File.binread(File.expand_path('../../../spec/fileformat/fixtures/unparsable_definitions.jed', __dir__))
    JedNative.from_image(fixture) do |db|
      reject_without_work(db, 'UPDATE c SET v = 1 WHERE id = 999')
      reject_without_work(db, 'DELETE FROM x WHERE id = 999')
      reject_without_work(db, 'INSERT INTO d(id) SELECT id FROM t WHERE id = 999')
      db.execute('CREATE OR REPLACE VIEW u AS SELECT id, v FROM t')
      assert(db.query('SELECT id FROM w ORDER BY id').rows == [[1], [2], [3]], 'replacement did not restore dependent view')
      db.execute('ALTER VIEW r RENAME TO renamed')
      reject_without_work(db, 'SELECT * FROM renamed')
      state = db.engine.send(:export_storage)
      renamed = state[:views].find { |view| view[:name] == 'renamed' }
      assert(renamed[:definition] == 'select id from t where id is future syntax', 'opaque view text changed')
      checks = state[:tables].find { |table| table[:name] == 'c' }[:checks]
      assert(checks.find { |check| check[:name] == 'c_v_check' }[:expr] == 'v is future syntax', 'opaque check text changed')
      index = state[:tables].find { |table| table[:name] == 'x' }[:indexes].find { |item| item[:name] == 'x_future_idx' }
      assert(index[:cols].first[:expr] == 'v is future syntax', 'opaque index text changed')
    end
    puts 'ruby-native opaque definitions: image loading, zero-work rejection, dependent replacement and verbatim metadata passed'
  end
end

NativeOpaqueDefinitionTests.run
