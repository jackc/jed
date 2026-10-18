# frozen_string_literal: true

require_relative 'persistence'
require_relative 'memory_storage'

module JedNative
  module StorageLimits
    attr_accessor :max_temp_storage_bytes

    def initialize(max_storage_bytes: 0, max_temp_storage_bytes: 0, page_size: 8192, **options)
      @native_page_size = page_size
      @storage_limits = { 'main' => [max_storage_bytes, 0].max }
      @max_temp_storage_bytes = max_temp_storage_bytes
      @memory_domains = {}
      super(**options)
    end

    def set_max_storage_bytes(bytes, database = 'main')
      raise Error.new('22023', 'max_storage_bytes must be an integer') unless bytes.is_a?(Integer)
      raise Error.new('42704', 'database is not attached') unless database == 'main' || @attachments.key?(database)
      bytes = [bytes, 0].max
      @storage_limits[database] = bytes
      storage = database == 'main' ? @storage : @attachments[database][:storage]
      storage.max_storage_bytes = bytes if storage
      nil
    end

    def seed_memory_image(bytes, state)
      @memory_domains['main'] = MemoryStorage.new(statistics_codec, page_size: storage_page_size, image: bytes, state: state)
    end

    def dispatch(statement)
      result = super
      if temporary_statement?(statement) && !%i[drop_table drop_view drop_index drop_sequence].include?(statement[:kind]) && @max_temp_storage_bytes.positive?
        size = @memory_domains['temp']&.page_count.to_i * 8192
        raise Error.new('54P03', 'temporary storage limit exceeded') if size > @max_temp_storage_bytes
      end
      result
    end

    def persist
      states = {}
      states['main'] = export_storage unless @storage
      @attachments.each do |name, attachment|
        states[name] = export_attachment_storage(name) unless attachment[:storage]
      end
      temporary = @tables.values.any? { |table| table[:temporary] } || @memory_domains.key?('temp')
      states['temp'] = export_temp_storage if temporary
      staged = []
      states.each do |name, state|
        domain = @memory_domains[name] ||= MemoryStorage.new(statistics_codec, page_size: name == 'main' ? storage_page_size : 8192)
        next if storage_signature(state) == storage_signature(domain.state)
        limit = name == 'temp' ? 0 : @storage_limits.fetch(name, 0)
        plan = domain.prepare(state, limit: limit, stages_rows: storage_stages_rows?(domain.state, state))
        staged << [domain, plan, state]
      end
      super
      staged.each { |domain, plan, state| domain.adopt(plan, state) }
    end

    private

    def export_temp_storage
      base = export_storage
      base.merge(tables: @tables.values.select { |table| table[:temporary] }.map { |table| export_table(table) },
                 sequences: [], views: @views.select { |_, view| view[:temporary] }.map { |name, view| export_view(name, view) })
    end

    def storage_stages_rows?(previous, current)
      old_tables = previous[:tables].to_h { |table| [table[:name], table] }
      codec = statistics_codec
      current[:tables].any? do |table|
        old = old_tables[table[:name]]
        next !table[:rows].empty? unless old
        prior = codec.table_entries(old).to_h
        codec.table_entries(table).any? { |key, row| prior[key] != row }
      end
    end
  end
  Engine.prepend(StorageLimits)
end
