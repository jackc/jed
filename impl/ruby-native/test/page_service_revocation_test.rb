# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

module RubyPageServiceRevocationTest
  # A deterministic transport boundary: the real encoder prepares a commit,
  # while the owner reports the protocol's terminal idle-write error.
  class RevokedConnection
    attr_reader :messages
    def initialize = @messages = []
    def request(name, _fields = {}, reply: true, **_keywords)
      @messages << name
      raise JedNative::Error.new('25P03', 'idle write transaction was revoked') if %i[commit_end read_pages].include?(name)
      raise "unexpected request #{name}" if reply
    end
  end

  def self.expect_revoked
    yield
    raise 'revoked write succeeded'
  rescue JedNative::Error => error
    raise unless error.sqlstate == '25P03'
  end

  def self.client(storage)
    token = Object.new
    connection = RevokedConnection.new
    client = JedNative::PageServiceClient.allocate
    { storage: storage, codec: storage.instance_variable_get(:@codec),
      page_size: storage.page_size, connection: connection,
      image: storage.instance_variable_get(:@image), state_mutex: Mutex.new,
      write_token: token, write_txid: storage.meta[:txid], counts: {}, grant: false }.each do |key, value|
      client.instance_variable_set("@#{key}", value)
    end
    [client, token, connection]
  end

  def self.run
    Dir.mktmpdir('jed-ruby-revocation') do |directory|
      storage = JedNative::Storage.new(path: File.join(directory, 'db.jed'), create: true, locking: :exclusive)
      begin
        %i[commit read_pages].each do |operation|
          client, token, connection = client(storage)
          expect_revoked do
            operation == :commit ? client.commit(storage.instance_variable_get(:@snapshot)) : client.read_pages([2])
          end
          client.end_write(token)
          raise "#{operation} sent abort after owner ended the write" if connection.messages.include?(:abort)
        end
        client, token, connection = client(storage)
        client.end_write(token)
        raise 'ordinary rollback omitted abort' unless connection.messages == [:abort]
      ensure
        storage.close
      end
    end

    owner = JedNative::PageServiceOwner::OwnerConnection.allocate
    owner.instance_variable_set(:@revoked, 7)
    expect_revoked { owner.dispatch(:read_pages, 1, txid: 7, indices: [2]) }
    raise 'owner retained a consumed revocation' if owner.instance_variable_get(:@revoked)
    puts 'ruby-native page service: terminal revocation releases client state without an extra abort'
  end
end

RubyPageServiceRevocationTest.run
