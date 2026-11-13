# frozen_string_literal: true

module JedNative
  # Host exports use a pinned committed snapshot and the same from-scratch
  # writer as compaction. Pending writes and session temporary objects never
  # enter the image.
  module Serialization
    def serialize(name = 'main')
      name = String(name)
      reader = read_session
      engine = reader.engine
      attachment = engine.instance_variable_get(:@attachments)[name] unless name == 'main'
      raise Error.new('42704', 'database is not attached') if name == 'temp' || name != 'main' && !attachment
      storage = attachment ? attachment[:storage] : engine.instance_variable_get(:@storage)
      raise Error.new('0A000', 'encrypted database export is not supported') if storage&.encrypted?
      domain = engine.instance_variable_get(:@memory_domains)[name]
      size = storage&.page_size || domain&.page_size || (name == 'main' ? page_size : 8192)
      state = attachment ? engine.export_attachment_storage(name) : engine.export_storage
      version = reader.instance_variable_get(:@serialization_versions).fetch(name, 0)
      codec = engine.send(:statistics_codec)
      pages = PageBuffer.new(size, 3)
      meta = PhysicalImage.new(codec, size).write(state, version) do |page, bytes|
        pages.resize(page + 1) if page >= pages.page_count
        pages[page * size, size] = bytes
      end
      pages.resize(meta[:page_count])
      pages.to_s
    ensure
      reader&.close
    end

  end
  Database.prepend(Serialization)

  def self.deserialize(bytes, **options, &block)
    from_image(bytes, **options, &block)
  end

  class Database
    def self.deserialize(bytes, **options, &block)
      JedNative.deserialize(bytes, **options, &block)
    end
  end
end
