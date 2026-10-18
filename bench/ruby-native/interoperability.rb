#!/usr/bin/env ruby
# frozen_string_literal: true

require 'tmpdir'
require 'fileutils'
require_relative '../../impl/ruby-native/lib/jed_native'

root = File.expand_path('../..', __dir__)
build = File.join(Dir.tmpdir, 'jed-ruby-native-compare')
FileUtils.mkdir_p(build)
binary = File.join(build, 'go-lane')
unless system({ 'GOCACHE' => File.join(build, 'go-cache'), 'GOPROXY' => 'off', 'GOSUMDB' => 'off' },
              'go', '-C', File.join(__dir__, 'compare/go'), 'build', '-o', binary, '.')
  abort 'Go interop driver build failed'
end

Dir.mktmpdir('jed-ruby-interop') do |directory|
  path = File.join(directory, 'handoff.jed')
  db = JedNative.create(path, page_size: 256)
  db.execute('CREATE TABLE t (id i32 PRIMARY KEY, v text, n decimal(9,2))')
  db.execute('CREATE INDEX t_n ON t (n DESC)')
  db.execute('CREATE VIEW visible AS SELECT id,v FROM t')
  db.execute('INSERT INTO t VALUES ($1,$2,$3)', 1, 'ruby', JedNative::Decimal.parse('12.50'))
  db.execute('INSERT INTO t VALUES ($1,$2,$3)', 2, 'overflow' * 500, nil)
  db.close
  abort 'Go could not adopt Ruby file' unless system(binary, '--interop', path, 'update')
  db = JedNative.open(path)
  actual = db.query('SELECT id,v,n FROM t ORDER BY id').rows.map { |row| row.map { |value| value&.to_s } }
  expected = [['1', 'ruby', '12.50'], ['2', 'from-go', nil], ['3', 'go', '123.45']]
  raise "Ruby could not adopt Go generation: #{actual.inspect}" unless actual == expected

  db.execute("UPDATE t SET v = 'ruby-again' WHERE id = 1")
  db.close
  abort 'Go could not read Ruby recommit' unless system(binary, '--interop', path, 'final')
end
puts 'Ruby → Go → Ruby → Go durable file handoff passed'
