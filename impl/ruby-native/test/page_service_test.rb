# frozen_string_literal: true

require 'json'
require 'tmpdir'
require 'open3'
require 'rbconfig'
require_relative '../lib/jed_native'
require_relative '../lib/jed_native/page_service_wire'

module RubyPageServiceTest
  Wire = JedNative::PageServiceWire
  def self.assert(value, message) = (raise message unless value)
  def self.page(text, size)
    prefix, fill = text.split('/', 2)
    bytes = [prefix].pack('H*')
    bytes + [fill].pack('H*') * (size - bytes.bytesize)
  end
  def self.value(type, text, size)
    case type
    when :u8, :u16, :u32, :u64 then text.to_i
    when :bool then text == 'true'
    when :id16 then [text].pack('H*')
    when :page then page(text, size)
    when :u32_list then text.split(',').map(&:to_i)
    when :page_list then text.split(';').map { |item| page(item, size) }
    when :digest_list then text.split(',').map { |item| item.split('=', 2).map(&:to_i) }
    when :credential
      kind, data = text.split(':', 2)
      { nil => nil, 'passphrase' => [:passphrase, data], 'key' => [:key, [data].pack('H*')] }.fetch(kind)
    when :indexed_page_list then text.split(';').map { |item| index, bytes = item.split('=', 2); [index.to_i, page(bytes, size)] }
    else text
    end
  end
  def self.fixtures
    # This fixture file deliberately uses only TOML string assignments. Read its
    # authored bytes directly, without making the core depend on a TOML gem.
    fixtures = []
    File.foreach(File.expand_path('../../../spec/pageservice/fixtures.toml', __dir__)) do |line|
      if (header = /\A\[\[([a-z_]+)\]\]/.match(line))
        fixtures << { kind: header[1] }
      elsif !fixtures.empty? && (field = /\A([a-z_]+)\s*=\s*(".*")\s*\z/.match(line))
        fixtures.last[field[1]] = JSON.parse(field[2])
      end
    end
    codec = JedNative::FileFormat.new
    fixtures.each do |fixture|
      bytes, size = [fixture.fetch('hex')].pack('H*'), fixture.fetch('page_size', '0').to_i
      case fixture[:kind]
      when 'frame'
        name = fixture.fetch('message').to_sym
        fields = Wire::MESSAGES.fetch(name)[1].to_h { |key, type| [key, value(type, fixture.fetch("f_#{key}"), size)] }
        request = fixture.fetch('request_id').to_i
        assert(Wire.encode(name, request, fields, size) == bytes, "wire encode #{fixture['name']}")
        assert(Wire.decode(bytes, size) == [name, request, fields], "wire decode #{fixture['name']}")
      when 'owner_record'
        fields = Wire::OWNER_FIELDS.to_h { |key, type| [key, value(type, fixture.fetch("f_#{key}"), size)] }
        assert(Wire.encode_owner(fields, codec) == bytes, "owner encode #{fixture['name']}")
        assert(Wire.decode_owner(bytes, codec) == fields, "owner decode #{fixture['name']}")
      when 'malformed'
        begin
          Wire.decode(bytes, size)
        rescue JedNative::Error => error
          assert(error.sqlstate == '08P01', "malformed state #{fixture['name']}")
        else
          raise "accepted malformed #{fixture['name']}"
        end
      end
    end
    puts "ruby-native page service: #{fixtures.length} shared wire fixtures passed"
  end

  def self.local_process
    Dir.mktmpdir('jed-ruby-service') do |directory|
      path = File.join(directory, 'db.jed')
      database = JedNative.create(path, locking: :auto)
      database.execute('CREATE TABLE t (id i32 PRIMARY KEY, v text)')
      database.execute("INSERT INTO t VALUES (1,'owner')")
      child_source = <<~'RUBY'
        require 'json'
        STDOUT.sync = true
        database = JedNative.open(ARGV.fetch(0), locking: :auto)
        session = database.session
        prepared = {}
        puts JSON.generate(role: database.engine.instance_variable_get(:@storage).service_role)
        STDIN.each_line do |line|
          break if line.chomp == 'close'
          begin
            result = session.execute_prepared(prepared[line] ||= session.prepare(line))
            puts JSON.generate(rows: result.rows, cost: result.cost)
          rescue JedNative::Error => error
            puts JSON.generate(error: error.sqlstate, message: error.message)
          end
        end
        session.close
        database.close
      RUBY
      library = File.expand_path('../lib', __dir__)
      Open3.popen3(RbConfig.ruby, "-I#{library}", '-rjed_native', '-e', child_source, path) do |input, output, errors, child|
        diagnostics = Thread.new { errors.read rescue '' }
        first = output.gets
        raise "client failed to open: #{diagnostics.value}" unless first
        assert(JSON.parse(first)['role'] == 'client', 'second process did not become a page client')
        command = lambda do |sql|
          input.puts(sql)
          input.flush
          line = output.gets
          raise "client ended: #{diagnostics.value}" unless line
          result = JSON.parse(line)
          raise "client SQL failed: #{result.inspect}" if result['error']
          result
        end
        assert(command.call('SELECT v FROM t WHERE id=1')['rows'] == [['owner']], 'client did not read owner pages')
        command.call("INSERT INTO t VALUES (2,'client')")
        assert(database.query('SELECT v FROM t WHERE id=2').rows == [['client']], 'owner did not adopt client commit')
        database.execute("INSERT INTO t VALUES (3,'new-owner')")
        assert(command.call('SELECT v FROM t WHERE id=3')['rows'] == [['new-owner']], 'client did not refresh after owner commit')
        command.call('BEGIN READ ONLY')
        database.execute("UPDATE t SET v='after-pin' WHERE id=1")
        assert(command.call('SELECT v FROM t WHERE id=1')['rows'] == [['owner']], 'client read pin changed')
        command.call('COMMIT')
        assert(command.call('SELECT v FROM t WHERE id=1')['rows'] == [['after-pin']], 'client did not release old snapshot')
        input.puts('close')
        input.close
        assert(child.value.success?, "client failed: #{diagnostics.value}")
      end
      database.close
    end
    puts 'ruby-native page service: owner/client reads, writes, refresh and pins passed'
  end
end

RubyPageServiceTest.fixtures
RubyPageServiceTest.local_process
