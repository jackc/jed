# frozen_string_literal: true

require 'thread'

module JedNative
  ScriptSummary = Struct.new(:statements_run, :rows_affected_total, :cost, keyword_init: true)
  StatementSpan = Struct.new(:text, :offset, keyword_init: true)

  # This scanner only finds boundaries. Parsing and limits remain per statement.
  def self.split_statements(sql)
    return enum_for(__method__, sql) unless block_given?
    start = i = 0
    content = false
    length = sql.bytesize
    while i < length
      byte = sql.getbyte(i)
      if byte == 45 && sql.getbyte(i + 1) == 45
        i += 2
        i += 1 while i < length && sql.getbyte(i) != 10
      elsif byte == 47 && sql.getbyte(i + 1) == 42
        depth = 1
        i += 2
        while i < length && depth.positive?
          if sql.byteslice(i, 2) == '/*'
            depth += 1
            i += 2
          elsif sql.byteslice(i, 2) == '*/'
            depth -= 1
            i += 2
          else
            i += 1
          end
        end
        raise Error.new('42601', 'unterminated comment') unless depth.zero?
      elsif byte == 39 || byte == 34
        content = true
        quote = byte
        escaped = quote == 39 && i.positive? && [69, 101].include?(sql.getbyte(i - 1))
        i += 1
        closed = false
        while i < length
          if escaped && sql.getbyte(i) == 92
            i += 2
          elsif sql.getbyte(i) == quote
            i += 1
            if sql.getbyte(i) == quote
              i += 1
            else
              closed = true
              break
            end
          else
            i += 1
          end
        end
        raise Error.new('42601', 'unterminated quoted string') unless closed
      elsif byte == 36 && (delimiter = /\A\$(?:[a-zA-Z_][a-zA-Z_0-9]*)?\$/.match(sql.byteslice(i, length - i)))
        content = true
        finish = sql.b.index(delimiter[0], i + delimiter[0].bytesize)
        raise Error.new('42601', 'unterminated dollar quote') unless finish
        i = finish + delimiter[0].bytesize
      elsif byte == 59
        yield StatementSpan.new(text: sql.byteslice(start, i - start), offset: start) if content
        i += 1
        start, content = i, false
      else
        content ||= ![9, 10, 11, 12, 13, 32].include?(byte)
        i += 1
      end
    end
    yield StatementSpan.new(text: sql.byteslice(start, i - start), offset: start) if content
  end

  module SessionVariables
    def initialize(**options)
      @variables = {}
      super
    end
    def variable_name(name) = String(name).tr('A-Z', 'a-z')
    def set_var(name, value)
      name = variable_name(name)
      raise Error.new('42704', 'custom setting requires a dotted name') unless name.include?('.')
      @variables[name] = String(value).dup
      nil
    end
    def reset_var(name)
      name = variable_name(name)
      raise Error.new('42704', 'custom setting requires a dotted name') unless name.include?('.')
      @variables.delete(name)
      nil
    end
    def reset_vars = @variables.clear
    def var(name) = @variables[variable_name(name)]&.dup
    def setting_value(name, missing_ok = false)
      return nil if name.nil?
      value = var(name)
      raise Error.new('42704', "unrecognized configuration parameter #{name}") if value.nil? && !missing_ok
      value
    end
  end
  Engine.prepend(SessionVariables)

  # Database owns publication and the writer gate; it never lends its Engine to
  # an executing session. Catalog containers are private, immutable row versions
  # and the file pager are shared. Readers retain their old roots until unpin.
  module SharedDatabase
    CATALOGS = %i[@tables @views @types @sequences].freeze
    STORAGE = %i[@storage @persisted_snapshot @storage_limits @native_page_size @storage_collations @default_collation].freeze

    def initialize(**options)
      @session_defaults = options.dup
      @session_setting_overrides = {}
      @shared_mutex, @writer_condition = Mutex.new, ConditionVariable.new
      # Bound methods retain only this database, not a capture frame containing
      # a session and its catalog roots. The watermark itself remains live.
      @shared_reader_watermark_provider = method(:shared_reader_watermark)
      @shared_storage_reuse_provider = method(:shared_storage_reuse_allowed?)
      @sessions, @pins = {}, {}
      @writer = nil
      @logical_version = @publication_generation = 0
      @database_closed = false
      super
      @session_defaults[:default_privileges] = @engine.default_privileges.dup.freeze if options.key?(:default_privileges)
    end

    def session(**options)
      defaults, overrides, grants, revokes = @shared_mutex.synchronize do
        shared_open!
        [@session_defaults.merge(options),
         @session_setting_overrides.reject { |name, _| options.key?(name == :timezone ? :time_zone : name) },
         @engine.instance_variable_get(:@privilege_grants).transform_values(&:dup),
         @engine.instance_variable_get(:@privilege_revokes).transform_values(&:dup)]
      end
      child = Session.new(self, **defaults)
      overrides.each { |name, value| child.send(:configure_session_setting, name, value) }
      child.engine.instance_variable_set(:@privilege_grants, grants)
      child.engine.instance_variable_set(:@privilege_revokes, revokes)
      child
    rescue Exception
      child&.close
      raise
    end
    def read_session(**options) = session(**options, read_only: true, pinned: true)
    def write_session(**options)
      child = session(**options)
      child.begin(true)
      child
    rescue Exception
      child&.close
      raise
    end
    def query(sql, *params) = with_session { |child| child.query(sql, *params) }
    def execute(sql, *params) = with_session { |child| child.execute(sql, *params) }
    def query_prepared(statement, *params) = with_session { |child| child.query_prepared(statement, *params) }
    def execute_prepared(statement, *params) = with_session { |child| child.execute_prepared(statement, *params) }
    def execute_script(sql) = with_session { |child| child.execute_script(sql) }
    def upgrade_collations = with_session { |child| child.upgrade_collations }
    def set_max_storage_bytes(bytes, database = 'main')
      @shared_mutex.synchronize do
        shared_open!
        super
      end
    end
    def storage_bytes(name = 'main')
      name = String(name)
      reader = read_session
      @shared_mutex.synchronize do
        shared_open!
        attachment = @engine.instance_variable_get(:@attachments)[name] unless name == 'main'
        raise Error.new('42704', 'database is not attached') unless name == 'main' || attachment
        storage = attachment ? attachment[:storage] : @engine.instance_variable_get(:@storage)
        if storage
          storage.meta.fetch(:live) * storage.page_size
        elsif (domain = @engine.instance_variable_get(:@memory_domains)[name])
          domain.page_count * domain.page_size
        else
          # Empty memory domains are allocated lazily, but logically already
          # contain both meta slots and the empty catalog page.
          3 * (name == 'main' ? page_size : 8192)
        end
      end
    ensure
      reader&.close
    end
    def to_image(page_size: self.page_size, txid: 0)
      reader = read_session
      image_from_engine(reader.engine, page_size, txid)
    ensure
      reader&.close
    end
    def transaction(&block) = with_session { |child| child.transaction(&block) }
    def view(&block) = with_session { |child| child.view(&block) }
    def update(&block) = with_session { |child| child.update(&block) }
    def with_session
      child = session
      yield child
    ensure
      child&.close
    end

    def current_logical_version = @shared_mutex.synchronize { @logical_version }
    alias version current_logical_version
    def oldest_live_txid = @shared_mutex.synchronize { @pins.values.map(&:first).min || @logical_version }
    def reader_watermark(file = false)
      @shared_mutex.synchronize do
        file ? (@pins.values.map(&:last).min || shared_file_txid + 1) : (@pins.values.map(&:first).min || @logical_version)
      end
    end
    def writer_held? = @shared_mutex.synchronize { !@writer.nil? }

    def shared_open!
      raise Error.new('55000', 'database is closed') if @database_closed
    end
    def shared_file_txid = @engine.instance_variable_get(:@storage)&.instance_variable_get(:@meta)&.fetch(:txid, 0).to_i
    def shared_tag
      storage = @engine.instance_variable_get(:@storage)
      storage ? shared_file_txid : @engine.instance_variable_get(:@memory_domains)['main']&.state&.object_id
    end
    def shared_capture(child, pin: false, read: false)
      @shared_mutex.synchronize do
        shared_open!
        destination = child.engine
        # Catalog copies are private to a session, but stay valid until a local
        # or foreign publication changes. Preserving their identity lets resolved
        # plans and index metadata survive repeated reads of the same roots.
        if child.instance_variable_get(:@captured_generation) != @publication_generation
          CATALOGS.each do |name|
            previous = destination.instance_variable_get(name) || {}
            temporary = previous.select { |_, value| value.is_a?(Hash) && value[:temporary] }
            shared = @engine.instance_variable_get(name)
            copy = name == :@types ? shared.dup : shared.transform_values(&:dup)
            destination.instance_variable_set(name, copy.merge(temporary))
          end
          attachments = @engine.instance_variable_get(:@attachments).transform_values(&:dup)
          destination.instance_variable_set(:@attachments, attachments)
        end
        STORAGE.each { |name| destination.instance_variable_set(name, @engine.instance_variable_get(name)) }
        domains = @engine.instance_variable_get(:@memory_domains).dup
        own_temp = destination.instance_variable_get(:@memory_domains)['temp']
        domains['temp'] = own_temp if own_temp
        destination.instance_variable_set(:@memory_domains, domains)
        destination.instance_variable_set(:@dirty, false)
        child.instance_variable_set(:@captured_generation, @publication_generation)
        @pins[child.object_id] = [@logical_version, shared_file_txid] if pin
        @sessions[child.object_id] = child
        @engine.instance_variable_get(:@storage)&.reader_watermark_provider = @shared_reader_watermark_provider
        destination.storage_reuse_allowed_provider = @shared_storage_reuse_provider
      end
    end
    def shared_pin(child)
      @shared_mutex.synchronize { @pins[child.object_id] ||= [@logical_version, shared_file_txid] }
    end
    def shared_unpin(child) = @shared_mutex.synchronize { @pins.delete(child.object_id) }
    def shared_acquire(child)
      deadline = child.lock_timeout_ms.positive? ? Process.clock_gettime(Process::CLOCK_MONOTONIC) + child.lock_timeout_ms / 1000.0 : nil
      @shared_mutex.synchronize do
        shared_open!
        while @writer && !@writer.equal?(child)
          remaining = deadline && deadline - Process.clock_gettime(Process::CLOCK_MONOTONIC)
          raise Error.new('55P03', 'writer lock timeout') if remaining && remaining <= 0
          @writer_condition.wait(@shared_mutex, remaining)
          shared_open!
        end
        @writer = child
        @pins.delete(child.object_id)
      end
    end
    def shared_publish(child)
      @shared_mutex.synchronize do
        before = @main_publication_tag
        CATALOGS.each do |name|
          values = child.engine.instance_variable_get(name)
          values = values.reject { |_, value| value.is_a?(Hash) && value[:temporary] }
          @engine.instance_variable_set(name, name == :@types ? values.dup : values.transform_values(&:dup))
        end
        STORAGE.each { |name| @engine.instance_variable_set(name, child.engine.instance_variable_get(name)) }
        @engine.instance_variable_set(:@memory_domains, child.engine.instance_variable_get(:@memory_domains).reject { |name, _| name == 'temp' })
        @engine.instance_variable_set(:@attachments, child.engine.instance_variable_get(:@attachments).transform_values(&:dup))
        current = shared_tag
        @logical_version += 1 if before != current
        @main_publication_tag = current
        @publication_generation += 1
      end
    end
    def shared_release(child)
      @shared_mutex.synchronize do
        if @writer.equal?(child)
          @writer = nil
          @writer_condition.broadcast
        end
      end
    end
    def shared_register(child)
      @shared_mutex.synchronize { @main_publication_tag = shared_tag if @sessions.empty? && @main_publication_tag.nil? }
      shared_capture(child, pin: child.pinned?)
    end
    def shared_forget(child)
      shared_unpin(child)
      shared_release(child)
      @shared_mutex.synchronize { @sessions.delete(child.object_id) }
    end
    def attach(name, source = :memory, mode = :read_write, **options)
      @shared_mutex.synchronize do
        raise Error.new('55006', 'attachment change while sessions are pinned') unless @pins.empty? && @writer.nil?
        result = super
        @publication_generation += 1
        result
      end
    end
    def detach(name)
      @shared_mutex.synchronize do
        raise Error.new('55006', 'attachment is in use') unless @pins.empty? && @writer.nil?
        result = super
        @publication_generation += 1
        result
      end
    end
    def close
      return if @database_closed
      @sessions.values.dup.each(&:close)
      @shared_mutex.synchronize do
        @database_closed = true
        @writer_condition.broadcast
      end
      super
    end

    private

    def shared_reader_watermark = reader_watermark(true)
    def shared_storage_reuse_allowed? = oldest_live_txid >= current_logical_version

    def configure_session_setting(name, value)
      @shared_mutex.synchronize do
        shared_open!
        applied = apply_session_setting(name, value)
        applied = applied.dup.freeze if applied.is_a?(Array) || applied.is_a?(String)
        @session_defaults[name == :timezone ? :time_zone : name] = applied
        # Constructor work_mem: 0 means default; the setter's 0 means unlimited.
        # Apply explicit setters after construction to preserve that distinction.
        @session_setting_overrides[name] = applied
        applied
      end
    end

    def configure_privileges(operation, *arguments)
      @shared_mutex.synchronize do
        shared_open!
        super
      end
    end
  end

  class Session < Database
    attr_reader :database
    attr_accessor :lock_timeout_ms

    def initialize(database, read_only: false, pinned: false, lock_timeout_ms: 0, time_zone: nil, **options)
      @database, @session_read_only, @pinned = database, read_only || database.read_only?, pinned
      @lock_timeout_ms, @session_closed, @gate = lock_timeout_ms, false, false
      # Database initialization is intentionally bypassed: a session is not a
      # second owner of storage or of a writer gate.
      @max_sql_length = options.delete(:max_sql_length) || 1_048_576
      @engine = Engine.new(**options)
      @engine.timezone = time_zone if time_zone
      @database.shared_register(self)
      @engine.instance_variable_set(:@read_only, true) if @session_read_only
    end
    def pinned? = @pinned
    def read_only? = @session_read_only
    def lifetime_cost = @engine.lifetime_cost
    def status = @engine.instance_variable_get(:@failed) ? :failed : (@engine.transaction_open? ? :open : :idle)
    def set_var(name, value) = @engine.set_var(name, value)
    def reset_var(name) = @engine.reset_var(name)
    def var(name) = @engine.var(name)
    def set_max_storage_bytes(bytes, database = 'main')
      raise Error.new('55000', 'session is closed') if @session_closed
      @database.set_max_storage_bytes(bytes, database)
    end
    def storage_bytes(name = 'main')
      raise Error.new('55000', 'session is closed') if @session_closed
      @database.storage_bytes(name)
    end
    def to_image(page_size: self.page_size, txid: 0)
      raise Error.new('55000', 'session is closed') if @session_closed
      @database.shared_open!
      return @database.to_image(page_size: page_size, txid: txid) unless @pinned || @engine.transaction_open?
      # A transaction's working set is private. Serialize the committed view
      # captured at BEGIN, leaving pending rows, DDL, and temporary state out.
      snapshot = @engine.dup
      snapshot.restore(@engine.instance_variable_get(:@transaction)) if @engine.transaction_open?
      image_from_engine(snapshot, page_size, txid)
    end
    def begin(writable = nil)
      query(writable.nil? ? 'BEGIN' : writable ? 'BEGIN READ WRITE' : 'BEGIN READ ONLY')
      self
    end
    def commit = query('COMMIT')
    def rollback = query('ROLLBACK')
    def query(sql, *params)
      query_prepared(prepare(sql), *params)
    rescue Error
      @engine.instance_variable_set(:@failed, true) if @engine.transaction_open? && !$!.sqlstate.eql?('25001')
      raise
    end
    def execute(sql, *params) = query(sql, *params)
    def query_prepared(statement, *params)
      raise Error.new('55000', 'session is closed') if @session_closed
      @database.shared_open!
      ast, was_open = statement.ast, @engine.transaction_open?
      kind = ast[:kind]
      if @session_read_only && kind == :begin && !was_open
        raise Error.new('25006', 'read-only session') if ast[:mode] == :read_write
        ast = ast.merge(mode: :read_only, read_only: true)
      end
      writing = statement_writes?(ast)
      if @session_read_only && writing && !@engine.temporary_statement?(ast)
        raise Error.new('25006', 'read-only session')
      end
      if !was_open && !@pinned
        if writing || kind == :begin && ast[:mode] == :read_write
          @database.shared_acquire(self)
          @gate = true
        end
        @database.shared_capture(self, pin: !@gate)
      elsif writing && !@gate
        @database.shared_acquire(self)
        @gate = true
        if @captured_generation != @database.instance_variable_get(:@publication_generation)
          # A pinned block may not overwrite a root published since it began.
          @engine.instance_variable_set(:@failed, true)
          raise Error.new('40001', 'transaction snapshot is no longer current')
        end
      end
      @engine.instance_variable_set(:@read_only, true) if @session_read_only
      @engine.instance_variable_set(:@read_only, false) if @session_read_only && kind == :begin && !was_open
      result = @engine.run(ast, params.map { |value| HostValues.copy(value) }).detach
      if !@engine.transaction_open?
        @database.shared_publish(self) if @gate && kind != :rollback
        @database.shared_release(self)
        @gate = false
        @database.shared_unpin(self) unless @pinned
      end
      result
    rescue Exception
      unless @engine.transaction_open?
        @database.shared_release(self)
        @gate = false
        @database.shared_unpin(self) unless @pinned
      end
      raise
    end
    def execute_prepared(statement, *params) = query_prepared(statement, *params)

    def statement_writes?(node)
      return false unless node.is_a?(Hash)
      kind = node[:kind]
      return false if kind == :explain && !node[:analyze]
      return true if %i[insert update delete truncate create_table drop_table alter_table create_view alter_view drop_view create_type drop_type alter_type create_index drop_index reindex analyze create_sequence drop_sequence alter_sequence].include?(kind)
      return true if kind == :call && %w[nextval setval].include?(node[:name])
      node.any? do |_, value|
        value.is_a?(Hash) ? statement_writes?(value) : value.is_a?(Array) && value.any? { |child| child.is_a?(Hash) && statement_writes?(child) }
      end
    end
    def execute_script(sql)
      owns = !@engine.transaction_open?
      self.begin if owns
      summary = ScriptSummary.new(statements_run: 0, rows_affected_total: 0, cost: 0)
      JedNative.split_statements(sql).each do |span|
        statement = prepare(span.text)
        if %i[begin commit rollback rollback_to savepoint release].include?(statement.ast[:kind])
          @engine.instance_variable_set(:@failed, true)
          raise Error.new('0A000', 'transaction control in execute_script')
        end
        result = execute_prepared(statement)
        summary.statements_run += 1
        summary.rows_affected_total += result.rows_affected.to_i
        summary.cost += result.cost
      end
      commit if owns
      summary
    rescue Exception
      @engine.instance_variable_set(:@failed, true) if @engine.transaction_open?
      rollback if owns && @engine.transaction_open?
      raise
    end
    def transaction(writable = nil)
      self.begin(writable)
      owns = true
      result = yield self
      commit
      result
    rescue Exception
      rollback if owns && @engine.transaction_open?
      raise
    end
    def view(&block) = transaction(false, &block)
    def update(&block) = transaction(true, &block)
    def attach(name, source = :memory, mode = :read_write, **options)
      raise Error.new('25001', 'cannot attach during transaction') if @engine.transaction_open?
      @database.attach(name, source, mode, **options)
      @database.shared_capture(self, pin: @pinned)
    end
    def detach(name)
      raise Error.new('55006', 'cannot detach during transaction') if @engine.transaction_open?
      @database.detach(name)
      @database.shared_capture(self, pin: @pinned)
    end
    def upgrade_collations
      raise Error.new('55000', 'session is closed') if @session_closed
      raise Error.new('25006', 'read-only session or transaction') if @session_read_only || @engine.instance_variable_get(:@read_only)
      unless @gate
        @database.shared_acquire(self)
        @gate = true
        @database.shared_capture(self) unless @engine.transaction_open? || @pinned
      end
      result = @engine.upgrade_collations
      @database.shared_publish(self) unless @engine.transaction_open?
      result
    ensure
      unless @engine.transaction_open?
        @database.shared_release(self)
        @gate = false
      end
    end
    def close
      return if @session_closed
      # Cleanup cannot consume a spent session allowance or keep its writer
      # gate held merely because SQL admission is now closed. ROLLBACK itself
      # is unmetered and never refunds the work already performed.
      budget = @engine.lifetime_max_cost
      begin
        @engine.lifetime_max_cost = 0
        @engine.run(kind: :rollback) if @engine.transaction_open?
      ensure
        @engine.lifetime_max_cost = budget
        @session_closed = true
        @engine.instance_variable_set(:@closed, true)
        @database.shared_forget(self)
      end
      nil
    end

    private

    def configure_session_setting(name, value)
      raise Error.new('55000', 'session is closed') if @session_closed
      apply_session_setting(name, value)
    end

    def configure_privileges(operation, *arguments)
      raise Error.new('55000', 'session is closed') if @session_closed
      @engine.public_send(operation, *arguments)
    end
  end
  Database.prepend(SharedDatabase)
end
