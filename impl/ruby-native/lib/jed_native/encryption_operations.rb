# frozen_string_literal: true

module JedNative
  # The handle's scratch_encryption setting (spec/design/encryption.md §6; encrypted.md §11): fixed
  # at open, `auto` (sealed when the main database is encrypted) or `on`.
  module ScratchEncryptionSetting
    def initialize(scratch_encryption: :auto, **options)
      setting = scratch_encryption.to_s.downcase.to_sym
      raise Error.new('22023', 'scratch_encryption must be auto or on') unless %i[auto on].include?(setting)
      @scratch_encryption = setting
      super(**options)
    end

    attr_reader :scratch_encryption

    def scratch_sealed? = @scratch_encryption == :on || !!@storage&.encrypted?

    # The scratch stream: a key-entropy stream of the handle's own, so scratch keys never perturb a
    # database's stream (encrypted.md §11).
    def scratch_stream
      @scratch_stream ||= (@environment ||= Environment.new).key_stream
    end
  end
  Engine.prepend(ScratchEncryptionSetting)

  # Credential change and rekey on a file database (spec/fileformat/encrypted.md §5, §12).
  module StorageEncryption
    # Rewrap the data key for a new credential, writing only the key block: the new copy over the
    # one not selected, then over the other, each followed by a barrier (encrypted.md §5).
    def change_credential(credential, kdf = nil)
      ensure_open(write: true)
      raise Error.new('55000', 'a page client cannot change its owner\'s credential') if @service_remote
      raise Error.new('28P03', 'the database is not encrypted') unless encrypted?
      raise Error.new('22023', 'a credential must be a JedNative::Credential') unless credential.is_a?(Credential)
      crypto = @codec.crypto
      # The KDF runs before the writer gate is taken.
      block = crypto.rewrap(credential, kdf)
      operation = lambda do
        @mutex.synchronize do
          ensure_open(write: true)
          pages = [2, 3].map { |page| @file.pread(@page_size, page * @page_size) }
          selected = KeyBlock.select(*pages)
          unless KeyBlock.generation(block) == KeyBlock.generation(selected) + 1
            raise Error.new('55006', 'the credential changed concurrently')
          end
          first = KeyBlock.unselected_page(*pages)
          bytes = block.ljust(@page_size, "\0")
          begin
            flush_pending(shrink: false)
            durable_sync(@file) if @sync
            if @needs_directory_sync
              sync_directory if @sync
              @needs_directory_sync = false
            end
            write_at(@file, bytes, first * @page_size)
            durable_sync(@file) if @sync
            write_at(@file, bytes, (5 - first) * @page_size)
            durable_sync(@file) if @sync
          rescue SystemCallError, IOError => error
            @poisoned = true
            @service_owner&.close
            raise Error.new('58030', error.message)
          end
          @image.invalidate([2, 3])
          crypto.key_block = block
        end
      end
      @service_owner ? @service_owner.with_exclusive_operation(&operation) : operation.call
      nil
    end

    # Rewrite the file under a new data key, encrypted for credential (nil: plaintext), every tree
    # rebuilt by ascending insertion at the target layout (encrypted.md §12). Like compaction it never
    # waits and replaces the file by rename.
    def rekey(credential, kdf = nil, scratch_sealed: false)
      ensure_open(write: true)
      raise Error.new('55006', 'a page client cannot rekey its owner') if @service_remote
      unless credential.nil? || credential.is_a?(Credential)
        raise Error.new('22023', 'a credential must be a JedNative::Credential')
      end
      if credential
        raise Error.new('55000', 'encrypting needs scratch_encryption = on') unless scratch_sealed
        raise Error.new('0A000', 'an encrypted database needs a page size of at least 1024') if @page_size < Crypto::MIN_PAGE_SIZE
      end
      # The target's key block (and its KDF) is derived before anything is held.
      random = encrypted? ? @codec.crypto.random : @key_random
      target = credential && PageCodec.create(credential, kdf, random)
      operation = lambda do
        flush(shrink: false)
        load
        @mutex.synchronize { rekey_file(target) }
      end
      @service_owner ? @service_owner.with_exclusive_operation(&operation) : operation.call
      nil
    end

    private

    # The committed snapshot with every value materialized and every physical tree dropped, so the
    # from-scratch writer rebuilds each tree at the target capacity.
    def rebuilt_snapshot
      tables = @snapshot.fetch(:tables, []).map do |table|
        entries = @codec.table_entries(table).map do |key, row|
          [key, row.map { |value| value.is_a?(StoredValue) ? value.resolve : value }]
        end
        indexes = table.fetch(:indexes, []).map { |index| index.reject { |key, _| %i[physical_tree physical_gist tree_profile gist_profile].include?(key) } }
        table.reject { |key, _| %i[physical_tree tree_profile].include?(key) }
             .merge(rows: entries.map(&:last), storage_keys: entries.map(&:first), indexes: indexes,
                    row_count: entries.length)
      end
      @snapshot.merge(tables: tables)
    end

    def rekey_file(target)
      writer = @codec.reader(crypto: target)
      state = rebuilt_snapshot
      writer.instance_variable_set(:@ctypes, state.fetch(:types, []).to_h { |type| [type[:name], type[:fields]] })
      image = begin
        writer.build_pages(state[:types] || [], state[:sequences] || [], state[:tables], @page_size,
                           state[:collations] || [], views: state[:views] || [], txid: @meta[:txid] + 1)
      rescue NotImplementedError => error
        raise Error.new('0A000', error.message)
      rescue Error
        raise
      rescue StandardError => error
        raise Error.new('54000', "cannot encode database: #{error.message}")
      end
      live = image.page_count - writer.first_body
      if @max_storage_bytes.positive? && live * @page_size > @max_storage_bytes
        raise Error.new('54P06', 'database storage limit exceeded')
      end
      temporary = "#{@path}.jedtmp"
      renamed = false
      begin
        mode = @file.stat.mode & 0o7777
        File.open(temporary, File::WRONLY | File::CREAT | File::TRUNC | File::NOFOLLOW, mode) do |output|
          output.chmod(mode)
          image.pages.keys.sort.each { |page| write_at(output, image.pages.fetch(page), page * @page_size) }
          output.truncate(image.page_count * @page_size)
          durable_sync(output) if @sync
        end
        @file.close
        File.rename(temporary, @path)
        renamed = true
        @file = File.open(@path, 'r+b')
        @file.close_on_exec = true
        sync_directory if @sync
        @codec.crypto = target
        @snapshot = decode_file
        @meta = @snapshot.fetch(:meta)
        @page_ids, @overflow_ids = {}.compare_by_identity, {}.compare_by_identity
        @free_pages, @free_generation = [], @meta[:txid]
        @live_at_compaction = @meta[:live]
        @needs_stabilization = @needs_directory_sync = false
        reset_durability_floor(@meta)
        @service_owner&.replace_image(@meta, @image.byteslice((@meta[:slot] || 0) * @page_size, @page_size))
        @meta
      rescue SystemCallError, IOError => error
        @poisoned = true if renamed
        if !renamed && @file.closed?
          begin
            @file = File.open(@path, 'r+b')
            @file.close_on_exec = true
          rescue SystemCallError, IOError
            @poisoned = true
          end
        end
        raise Error.new('58030', error.message)
      rescue Error
        @poisoned = true if renamed
        raise
      rescue StandardError => error
        @poisoned = true if renamed
        raise Error.new('XX001', error.message)
      ensure
        File.unlink(temporary) if File.exist?(temporary)
      end
    end
  end

  # Database#change_credential and Database#rekey (spec/design/api.md, "Encryption at rest").
  module DatabaseEncryption
    def change_credential(name, credential, kdf = nil) = encryption_operation(name) { |storage, _| storage.change_credential(credential, kdf) }

    def rekey(name, credential, kdf = nil)
      encryption_operation(name, rekey: true) do |storage, attachment|
        storage.rekey(credential, kdf, scratch_sealed: @engine.scratch_sealed?)
        @engine.send(:adopt_storage_pages, storage, attachment ? String(name) : nil)
        if attachment
          attachment[:storage_signature] = @engine.send(:storage_signature, @engine.export_attachment_storage(String(name)))
        else
          @engine.instance_variable_set(:@persisted_snapshot, @engine.send(:storage_signature, @engine.export_storage))
        end
        @service_seen ||= {}
        @service_seen[storage.path] = [storage.service_generation, storage.meta[:txid]]
        @logical_version += 1 unless attachment
        @publication_generation += 1
        @main_publication_tag = shared_tag
      end
    end

    private

    def encryption_operation(name, rekey: false)
      name = String(name)
      @shared_mutex.synchronize do
        shared_open!
        attachment = name == 'main' ? nil : @engine.instance_variable_get(:@attachments)[name]
        raise Error.new('42704', 'database is not attached') unless name == 'main' || attachment
        storage = attachment ? attachment[:storage] : @engine.instance_variable_get(:@storage)
        raise Error.new('55000', 'an in-memory database is not encrypted') unless storage
        raise Error.new('25006', 'database is read-only') if attachment&.dig(:read_only) || storage.read_only?
        raise Error.new('55006', 'database is in use') unless @writer.nil? && (!rekey || @pins.empty?)
        storage.load
        yield storage, attachment
      end
      nil
    end
  end
  Storage.prepend(StorageEncryption)
  Database.prepend(DatabaseEncryption)

  class << self
    alias plaintext_memory memory

    # An in-memory database cannot be encrypted (encrypted.md §9).
    def memory(**options, &block)
      raise Error.new('55000', 'an in-memory database cannot be encrypted') if options[:credential]
      plaintext_memory(**options, &block)
    end
  end
end
