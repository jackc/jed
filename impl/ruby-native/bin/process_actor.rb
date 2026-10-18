#!/usr/bin/env ruby
# frozen_string_literal: true

require_relative '../lib/jed_native'
STDOUT.sync = true

def reply_error(error)
  puts "ERR\t#{error.respond_to?(:sqlstate) ? error.sqlstate : 'XX000'}\t#{error.message.unpack1('H*')}"
end

begin
  action, path, *arguments = ARGV
  positional, named = arguments.partition { |argument| !argument.include?('=') }
  options = { locking: action == 'open-exclusive' ? :exclusive : :auto,
              file_lock_timeout_ms: positional[0] ? positional[0].to_i : 5000 }
  options[:idle_in_transaction_timeout_ms] = positional[1].to_i if positional[1]
  mapping = { 'yield_grace' => :yield_grace_ms, 'shutdown_grace' => :shutdown_grace_ms,
              'idle_session' => :idle_session_timeout_ms, 'max_connections' => :max_connections }
  named.each do |argument|
    key, value = argument.split('=', 2)
    options[key == 'ownership' ? :ownership : mapping.fetch(key)] = key == 'ownership' ? value.to_sym : value.to_i
  end
  options[:read_only] = true if action == 'open-read-only'
  database = action == 'create' ? JedNative.create(path, **options) : JedNative.open(path, **options)
  puts 'READY'
  reader = writer = nil
  STDIN.each_line do |line|
    command, argument = line.chomp.split("\t", 2)
    argument ||= ''
    value = ''
    begin
      sql = -> { [argument].pack('H*').force_encoding(Encoding::UTF_8) }
      case command
      when 'EXEC' then database.execute(sql.call)
      when 'QUERY_I64', 'READ_QUERY_I64'
        result = (command == 'QUERY_I64' ? database : reader).query(sql.call)
        value = result.rows.map { |row| row.map { |item| item.nil? ? 'NULL' : item.to_s }.join(':') }.join(',')
      when 'READ_OPEN' then reader = database.read_session
      when 'READ_CLOSE' then reader&.close; reader = nil
      when 'WRITE_OPEN'
        writer = database.session
        writer.lock_timeout_ms = argument.to_i
        writer.begin(true)
      when 'WRITE_EXEC' then writer.execute(sql.call)
      when 'WRITE_COMMIT' then writer.commit
      when 'WRITE_ROLLBACK' then writer.rollback
      when 'ATTACH'
        name, read_only, source = argument.split("\t", 3)
        database.attach(name, source, read_only == '1' ? :read_only : :read_write, locking: :auto)
      when 'DETACH' then database.detach(argument)
      when 'ROLE'
        storage = argument.empty? ? database.engine.instance_variable_get(:@storage) : database.engine.instance_variable_get(:@attachments).fetch(argument)[:storage]
        value = storage.service_role.to_s
      when 'TXID' then value = database.engine.instance_variable_get(:@storage).meta[:txid].to_s
      when 'PAGE_COUNT' then value = database.engine.instance_variable_get(:@storage).meta[:page_count].to_s
      when 'MAX_STORAGE_BYTES' then database.max_storage_bytes = argument.to_i
      when 'STORAGE_BYTES' then value = database.engine.instance_variable_get(:@storage).meta[:live].then { |pages| pages * database.engine.instance_variable_get(:@storage).page_size }.to_s
      when 'HANGUP_NEXT_ACK' then database.engine.instance_variable_get(:@storage).instance_variable_set(:@service_drop_ack, true)
      when 'CRASH_NEXT_ACK' then database.engine.instance_variable_get(:@storage).instance_variable_set(:@service_crash_ack, true)
      when 'COMPACT', 'BACKUP'
        raise JedNative::Error.new('0A000', "#{command} is not yet implemented")
      when 'CLOSE'
        reader&.close
        writer&.close
        puts 'OK'
        database.close
        database = nil
        break
      else raise JedNative::Error.new('0A000', "unknown process actor command #{command}")
      end
      puts value.empty? ? 'OK' : "OK\t#{value}"
    rescue StandardError => error
      reply_error(error)
    end
  end
rescue StandardError => error
  reply_error(error)
  exit 1
ensure
  database&.close
end
