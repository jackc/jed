# frozen_string_literal: true

require_relative '../lib/jed_native'
require 'tmpdir'
require 'open3'
require 'json'

Dir.mktmpdir('ruby-service-attachments') do |directory|
  main, auxiliary = %w[main.jed aux.jed].map { |name| File.join(directory, name) }
  JedNative.create(auxiliary) { |database| database.execute('create table t(id i64 primary key,v text)') }
  database = JedNative.create(main, locking: :auto)
  database.attach('aux', auxiliary)
  database.execute("insert into aux.t values(1,'first')")
  source = <<~'CHILD'
    STDOUT.sync = true
    database = JedNative.open(ARGV[0], locking: :auto)
    database.attach('aux', ARGV[1])
    session = database.session
    puts 'ready'
    STDIN.each_line do |sql|
      break if sql.chomp == 'close'
      begin
        puts JSON.generate(rows: session.execute(sql).rows)
      rescue JedNative::Error => error
        puts JSON.generate(error: error.sqlstate, message: error.message)
      end
    end
    database.close
  CHILD
  library = File.expand_path('../lib', __dir__)
  Open3.popen3(RbConfig.ruby, "-I#{library}", '-rjed_native', '-rjson', '-e', source, main, auxiliary) do |input, output, errors, child|
    diagnostics = Thread.new { errors.read }
    raise "attachment client failed: #{diagnostics.value}" unless output.gets&.chomp == 'ready'
    command = lambda do |sql|
      input.puts(sql)
      response = output.gets || raise("attachment client stopped: #{diagnostics.value}")
      value = JSON.parse(response)
      raise value.inspect if value['error']
      value['rows']
    end
    raise 'attachment initial read' unless command.call('select v from aux.t') == [['first']]
    main_txid = database.engine.instance_variable_get(:@storage).meta[:txid]
    command.call("insert into aux.t values(2,'client')")
    raise 'owner missed foreign attachment commit' unless database.execute('select v from aux.t where id=2').rows == [['client']]
    raise 'attachment commit rewrote main' unless database.engine.instance_variable_get(:@storage).meta[:txid] == main_txid
    command.call('begin read only')
    database.execute("update aux.t set v='changed' where id=1")
    raise 'attachment read pin moved' unless command.call('select v from aux.t where id=1') == [['first']]
    command.call('commit')
    raise 'attachment did not refresh after release' unless command.call('select v from aux.t where id=1') == [['changed']]
    input.puts('close')
    input.close
    status = child.value
    detail = diagnostics.value
    raise detail unless status.success?
  end
  database.close
end
puts 'ruby-native page-service attachments: inherited roles, independent commits and pinned refresh passed'
