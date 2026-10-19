# frozen_string_literal: true

module JedNative
  module StorageOperations
    def compact(name = 'main')
      name = String(name)
      @shared_mutex.synchronize do
        shared_open!
        attachment = name == 'main' ? nil : @engine.instance_variable_get(:@attachments)[name]
        raise Error.new('42704', 'database is not attached') unless name == 'main' || attachment
        storage = attachment ? attachment[:storage] : @engine.instance_variable_get(:@storage)
        raise Error.new('25006', 'database is read-only') if attachment&.dig(:read_only) || storage&.read_only?
        raise Error.new('55006', 'database is in use') unless @writer.nil? && @pins.empty?
        if storage
          storage.load
          storage.compact
          @engine.send(:adopt_storage_pages, storage, name == 'main' ? nil : name)
          if attachment
            attachment[:storage_signature] = @engine.send(:storage_signature, @engine.export_attachment_storage(name))
          else
            @engine.instance_variable_set(:@persisted_snapshot, @engine.send(:storage_signature, @engine.export_storage))
          end
          @service_seen ||= {}
          @service_seen[storage.path] = [storage.service_generation, storage.meta[:txid]]
        else
          compact_memory_domain(name, attachment)
        end
        @logical_version += 1 if name == 'main'
        @publication_generation += 1
        @main_publication_tag = shared_tag
      end
      nil
    end

    def backup(destination, form: :page_identical)
      form = form.to_sym
      raise Error.new('22023', 'unknown backup form') unless %i[page_identical compacted].include?(form)
      reader = read_session
      storage = @engine.instance_variable_get(:@storage)
      if storage
        storage.load
        storage.backup(destination, form: form)
      else
        raise Error.new('0A000', 'page-identical backup requires file storage') if form == :page_identical
        backup_memory_domain(reader.engine, destination)
      end
      nil
    ensure
      reader&.close
    end

    private

    def compact_memory_domain(name, attachment)
      state = attachment ? @engine.export_attachment_storage(name) : @engine.export_storage
      domains = @engine.instance_variable_get(:@memory_domains)
      previous = domains[name]
      page_size = previous&.page_size || @engine.send(:storage_page_size)
      codec = @engine.send(:statistics_codec)
      pages = PageBuffer.new(page_size, 3)
      meta = PhysicalImage.new(codec, page_size).write(state, @logical_version + 1) do |page, bytes|
        pages.resize(page + 1) if page >= pages.page_count
        pages[page * page_size, page_size] = bytes
      end
      pages.resize(meta[:page_count])
      image = pages.to_s
      decoded = FileFormat.new(collation_key: codec.instance_variable_get(:@collation_key)).decode_image(image, page_size: page_size, lazy: true)
      if attachment
        @engine.import_attachment_catalog(name, attachment, decoded)
      else
        others = @engine.tables.select { |_, table| table[:database] }
        @engine.send(:import_storage, decoded)
        @engine.tables.merge!(others)
      end
      compacted = attachment ? @engine.export_attachment_storage(name) : @engine.export_storage
      domains[name] = MemoryStorage.new(codec, page_size: page_size, image: image, state: compacted.merge(free_pages: [], live_pages: decoded[:live_pages]))
      @engine.refresh_primary_indexes
    end

    def backup_memory_domain(engine, destination)
      destination = File.join(File.realpath(File.dirname(destination)), File.basename(destination))
      raise Error.new('58P02', 'backup destination exists') if File.exist?(destination) || File.symlink?(destination)
      temporary = "#{destination}.jedtmp"
      page_size = engine.send(:storage_page_size)
      state = engine.export_storage
      codec = engine.send(:statistics_codec)
      File.open(temporary, File::WRONLY | File::CREAT | File::TRUNC | File::NOFOLLOW, 0o600) do |file|
        meta = PhysicalImage.new(codec, page_size).write(state, current_logical_version + 1) do |page, bytes|
          written = 0
          while written < bytes.bytesize
            count = file.pwrite(bytes.byteslice(written..), page * page_size + written)
            raise IOError, 'short backup write' if count.zero?
            written += count
          end
        end
        file.truncate(meta[:page_count] * page_size)
        file.fsync
        file.fcntl(51, 0) if RUBY_PLATFORM.include?('darwin')
      end
      File.link(temporary, destination)
      File.unlink(temporary)
      File.open(File.dirname(destination), File::RDONLY) { |directory| directory.fsync }
    rescue Errno::EEXIST
      raise Error.new('58P02', 'backup destination exists')
    rescue SystemCallError, IOError => error
      raise Error.new('58030', error.message)
    ensure
      File.unlink(temporary) if temporary && File.exist?(temporary)
    end
  end
  Database.prepend(StorageOperations)
end
