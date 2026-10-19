# frozen_string_literal: true

module JedNative
  module ServiceDatabase
    def service_storages(writable: false)
      main = @engine.instance_variable_get(:@storage)
      attachments = @engine.instance_variable_get(:@attachments)
      return main ? [main] : [] if attachments.empty?
      attached = attachments.values.filter_map { |attachment| attachment[:storage] unless writable && attachment[:read_only] }
      ([main].compact + attached).sort_by { |storage| storage.path.b }
    end

    def shared_capture(child, pin: false, read: false)
      if pin || read
        service_storages.each { |storage| storage.service_begin_read(child) unless child.instance_variable_get(:@service_gates)&.include?(storage) }
      end
      service_adopt_foreign
      super
      child.engine.instance_variable_set(:@service_roles, service_storages.to_h { |storage| [storage, storage.service_generation] })
    rescue Exception
      service_storages.each { |storage| storage.service_end_read(child) } unless child.pinned?
      raise
    end
    def shared_acquire(child)
      super
      acquired = []
      begin
        service_storages(writable: true).each do |storage|
          storage.service_acquire(child, child.lock_timeout_ms)
          acquired << storage
        end
        child.instance_variable_set(:@service_gates, acquired)
        service_adopt_foreign
      rescue Exception
        acquired.reverse_each { |storage| storage.service_release(child) }
        shared_release(child)
        raise
      end
    end
    def shared_release(child)
      gates = child.instance_variable_get(:@service_gates) || []
      child.instance_variable_set(:@service_gates, nil)
      gates.reverse_each { |storage| storage.service_release(child) }
      super
    end
    def shared_unpin(child)
      service_storages.each { |storage| storage.service_end_read(child) }
      super
    end
    def shared_cursor_pin(cursor, session)
      service_storages.each { |storage| storage.service_copy_pin(session, cursor) }
      super
    end
    def shared_publish(child)
      super
      @service_seen = service_storages.to_h { |storage| [storage.path, [storage.service_generation, storage.meta[:txid]]] }
    end

    private

    def service_adopt_foreign
      @service_seen ||= {}
      main = @engine.instance_variable_get(:@storage)
      attachments = @engine.instance_variable_get(:@attachments)
      domains = [[nil, main]].select { |_, storage| storage }
      domains.concat(attachments.filter_map { |name, attachment| [name, attachment[:storage]] if attachment[:storage] })
      domains.each do |name, storage|
        next if @service_seen.key?(storage.path)
        initial = name ? attachments[name][:service_publication] : @engine.instance_variable_get(:@storage_publication)
        @service_seen[storage.path] = initial || [storage.service_generation, storage.meta[:txid]]
      end
      changed = domains.select { |_, storage| @service_seen[storage.path] != [storage.service_generation, storage.meta[:txid]] }
      return if changed.empty?
      @shared_mutex.synchronize do
        changed.each do |name, storage|
          signature = [storage.service_generation, storage.meta[:txid]]
          next if @service_seen[storage.path] == signature
          if name
            attachment = attachments[name].dup
            @engine.import_attachment_catalog(name, attachment, storage.load)
            attachment[:service_publication] = signature
            attachments[name] = attachment
            @engine.remember_attachment_storage(name)
          else
            attached_tables = @engine.instance_variable_get(:@tables).select { |_, table| table[:database] }
            @engine.send(:import_storage, storage.load)
            @engine.instance_variable_set(:@storage_publication, signature)
            @engine.instance_variable_get(:@tables).merge!(attached_tables)
            @engine.instance_variable_set(:@persisted_snapshot, @engine.send(:storage_signature, @engine.export_storage))
            @logical_version += 1
            @main_publication_tag = shared_tag
          end
          @service_seen[storage.path] = signature
          @publication_generation += 1
        end
        @engine.refresh_primary_indexes
      end
    end
  end
  Database.prepend(ServiceDatabase)

  module ServiceRoleGuard
    def run(statement, *arguments)
      unless %i[rollback rollback_to].include?(statement[:kind])
        (@service_roles || {}).each do |storage, generation|
          storage.send(:ensure_open)
          raise Error.new('08006', 'transaction belongs to an earlier page-service role') unless storage.service_generation == generation
        end
      end
      super
    end
  end
  Engine.prepend(ServiceRoleGuard)
end
