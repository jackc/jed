# frozen_string_literal: true

require 'json'
require File.join(ENV.fetch('RUBY_NATIVE_SOURCE', File.expand_path('../../../impl/ruby-native', __dir__)), 'lib/jed_native')

database = JedNative.memory
# The archived first-working baseline predates the explicit Session surface.
db = database.respond_to?(:session) ? database.session : database
repetitions = validation_iterations = 0
verify_only = ARGV.include?('--verify-only')
File.foreach(ARGV.fetch(0)) do |line|
  fields = line.chomp.split("\t", -1)
  case fields.shift
  when 'config'
    repetitions, validation_iterations = fields.map(&:to_i)
  when 'setup'
    db.execute(fields.fetch(0))
  when 'workload'
    name, iterations, warmup, offset, step, span, sql = fields
    iterations, warmup, offset, step, span = [iterations, warmup, offset, step, span].map(&:to_i)
    prepared = db.prepare(sql)
    parameters = Array.new([validation_iterations, warmup, iterations].max) do |i|
      span.zero? ? [] : [offset + (i * step) % span]
    end
    checksum = 0xcbf29ce484222325
    validation_rows = 0
    validation_iterations.times do |i|
      result = db.query_prepared(prepared, *parameters[i])
      validation_rows += result.rows.length
      result.rows.each do |row|
        row.each do |value|
          bytes = value.nil? ? 'NULL' : value.to_s
          bytes.each_byte { |byte| checksum = ((checksum ^ byte) * 0x100000001b3) & 0xffffffffffffffff }
          checksum = ((checksum ^ 0x1f) * 0x100000001b3) & 0xffffffffffffffff
        end
        checksum = ((checksum ^ 0x1e) * 0x100000001b3) & 0xffffffffffffffff
      end
    end
    samples = []
    total_rows = 0
    drain = lambda do |i|
      count = 0
      db.query_prepared(prepared, *parameters[i]).rows.each { count += 1 }
      count
    end
    unless verify_only
      warmup.times { |i| drain.call(i) }
      repetitions.times do
        rows = 0
        started = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)
        iterations.times { |i| rows += drain.call(i) }
        samples << Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond) - started
        total_rows += rows
      end
    end
    puts JSON.generate(workload: name, checksum: format('%016x', checksum), validation_rows: validation_rows,
                       samples_ns: samples, rows_total: total_rows, iterations: iterations, warmup: warmup,
                       runtime: RUBY_DESCRIPTION)
    STDOUT.flush
  end
end
db.close
database.close unless db.equal?(database)
