# frozen_string_literal: true

module JedNative
  # Fiber suspension preserves the executor's ensure scopes, spill files and
  # live memory account between pulls. It does not run SQL on another thread.
  class CursorClosed < Exception; end

  class CursorOutput < Array
    def initialize(engine, columns)
      @engine, @columns, @count = engine, columns, 0
    end
    def <<(row)
      @count += 1
      @engine.cursor_yield(row, @columns)
      self
    end
    def length = @count
    alias size length
    def empty? = @count.zero?
  end

  module CursorExecution
    def cursor_metadata(statement)
      ast = route_domains(statement)
      schema = parameter_binding_schema
      context = [@environment, timezone, default_collation]
      @native_cursor_metadata ||= {}.compare_by_identity
      entry = @native_cursor_metadata[ast]
      unless entry && entry[0] == schema && entry[1] == context
        binding = ParameterBindings.new(self, ast)
        binding.resolve
        previous = @native_binding_types
        begin
          @native_binding_types = binding.types
          columns = query_metadata(ast)[:columns]
        ensure
          @native_binding_types = previous
        end
        @native_cursor_metadata.shift if @native_cursor_metadata.length >= 64
        entry = @native_cursor_metadata[ast] = [schema, context, binding, columns]
      end
      [ast, entry[2], entry[3]]
    end

    def select(statement)
      @cursor_select_depth = (@cursor_select_depth || 0) + 1 if @native_cursor
      super
    ensure
      @cursor_select_depth -= 1 if @native_cursor
    end
    def cursor_output? = @native_cursor && @cursor_select_depth == 1
    def cursor_output(columns) = cursor_output? ? CursorOutput.new(self, columns) : []
    def cursor_yield(row, columns)
      @native_cursor_emitted = true
      @native_cursor.accept_row(row, columns)
    end
    def cacheable_select?(statement)
      return false if cursor_output? && (!(statement[:from] || []).empty? || !(statement[:order_by] || []).empty?)
      super
    end
    def query_memory_collect_result?
      return false if cursor_output?
      super
    end
    def charge(amount = 1)
      @lifetime_cost = @cursor_session.engine.lifetime_cost if @cursor_session
      super
    ensure
      @cursor_session.engine.instance_variable_set(:@lifetime_cost, @lifetime_cost) if @cursor_session
    end
  end
  Engine.prepend(CursorExecution)

  class Rows
    include Enumerable
    attr_reader :columns, :column_types
    alias column_names columns
    SHARED_CACHES = %i[@native_read_plans @native_select_plans @native_cte_validation
                       @native_window_shapes @native_spill_shapes @native_stream_shapes].freeze
    REPLACED_STATE = (SharedDatabase::CATALOGS + %i[@native_binding_plans @savepoints
      @params @foreign_key_checks @native_query_shape_cache @subquery_hoists]).freeze

    def initialize(session, statement, params)
      @session, @database = session, session.database
      ast, binding, metadata = session.engine.cursor_metadata(statement.ast)
      raise Error.new('42601', 'bind parameter count mismatch') unless params.length == binding.count
      bound = binding.bind(params) unless binding.count.zero?
      prebound_types = binding.types if binding.reusable_bound_values?
      params = bound if prebound_types && bound
      SHARED_CACHES.each do |name|
        session.engine.instance_variable_set(name, {}.compare_by_identity) unless session.engine.instance_variable_defined?(name)
      end
      @engine = session.engine.dup
      @engine.instance_variables.each do |name|
        next if SHARED_CACHES.include?(name) || REPLACED_STATE.include?(name)
        value = @engine.instance_variable_get(name)
        @engine.instance_variable_set(name, value.dup) if value.is_a?(Hash) || value.is_a?(Array)
      end
      SharedDatabase::CATALOGS.each do |name|
        values = session.engine.instance_variable_get(name)
        # Reads never mutate published table/view descriptors. Their containers
        # remain private, while unchanged descriptors retain plan identity.
        @engine.instance_variable_set(name, name == :@sequences ? values.transform_values(&:dup) : values.dup)
      end
      @engine.instance_variable_set(:@transaction, nil)
      @engine.instance_variable_set(:@savepoints, [])
      @engine.instance_variable_set(:@dirty, false)
      @engine.instance_variable_set(:@closed, false)
      @engine.instance_variable_set(:@native_cursor, self)
      @engine.instance_variable_set(:@cursor_session, session)
      @engine.instance_variable_set(:@cost, 0)
      @engine.instance_variable_set(:@native_cursor_emitted, false)
      @engine.instance_variable_set(:@cursor_select_depth, 0)
      unless prebound_types
        binding = binding.dup
        binding.instance_variable_set(:@engine, @engine)
        @engine.instance_variable_set(:@native_binding_plans, { ast => [@engine.parameter_binding_schema, binding] }.compare_by_identity)
      end
      @columns = metadata.map { |column| column[:name].dup }
      @column_types = metadata.map { |column| column[:type].dup }
      @engine.send(:authorize_node, ast, {}, true, []) unless @engine.unrestricted_capabilities?
      @closed, @collecting, @cached_rows = false, false, nil
      @block_bound = session.engine.transaction_open?
      @database.shared_cursor_pin(self, session)
      session.register_cursor(self)
      @fiber = Fiber.new do
        begin
          if prebound_types
            @engine.instance_variable_set(:@native_binding_types, prebound_types)
            @engine.instance_variable_set(:@native_binding_active, true)
          end
          result = @engine.run(ast, params)
          unless @engine.instance_variable_get(:@native_cursor_emitted)
            result.rows.each { |row| accept_row(row, result.columns.zip(result.column_types).map { |name, type| { name: name, type: type } }) }
          end
        rescue CursorClosed
          # Closing a read cursor discards unread output without failing its block.
        rescue Exception
          session.engine.instance_variable_set(:@failed, true) if @block_bound && session.engine.transaction_open?
          raise
        ensure
          if prebound_types
            @engine.instance_variable_set(:@native_binding_types, nil)
            @engine.instance_variable_set(:@native_binding_active, false)
          end
          finish
        end
        nil
      end
    end

    def own_session! = (@owns_session = true; self)
    def collecting? = @collecting
    def block_bound? = @block_bound
    def closed? = @closed
    def cost = @engine ? @engine.cost : @final_cost
    def rows_affected = nil
    def accept_row(row, columns)
      if @collecting && @engine.instance_variable_get(:@query_memory)
        @collection_owner ||= @engine.query_memory_owner
        @engine.query_memory_add_row(@collection_owner, row, columns)
      end
      unless @row_names
        @row_names = columns.map { |column| column[:name].dup.freeze }.freeze
        @columns = @row_names.map(&:dup)
        @column_types = columns.map { |column| column[:type].dup }
      end
      Fiber.yield(Row.new(row.map { |value| HostValues.copy(value) }, @row_names))
    end
    def next
      row = pull
      raise StopIteration unless row
      row
    end
    # Enumerable drains use an ordinary sentinel at EOF. Only the public next
    # method needs an exception (and its backtrace) to report exhaustion.
    def pull
      return nil if @closed
      @started = true
      @fiber.resume
    rescue Exception
      finish unless @fiber&.alive?
      raise
    end
    private :pull
    def each
      return enum_for(:each) unless block_given?
      begin
        while (row = pull)
          yield row
        end
      ensure
        close
      end
      self
    end
    def rows
      return @cached_rows if @cached_rows
      @collecting = true
      @cached_rows = each.to_a
    end
    def empty? = rows.empty?
    def size = rows.size
    def close
      return if @closed
      if @started && @fiber&.alive? && @fiber.respond_to?(:raise)
        @fiber.raise(CursorClosed)
      else
        finish
      end
      nil
    end
    def finish
      return if @closed
      @closed = true
      @final_cost = @engine.cost
      begin
        @database.shared_unpin(self)
      ensure
        @session.unregister_cursor(self)
        @session.close if @owns_session
      end
    ensure
      if @closed
        # Collected rows and their metadata belong to the caller. Keeping that
        # result must not retain a closed cursor's old catalog/pager generation.
        @engine = @fiber = @session = @database = nil
      end
    end
  end

  module CursorDatabase
    def query(sql, *params)
      child = session
      result = child.query(sql, *params)
      result.is_a?(Rows) ? result.own_session! : (child.close; result)
    rescue Exception
      child&.close
      raise
    end
    def query_prepared(statement, *params)
      child = session
      result = child.query_prepared(statement, *params)
      result.is_a?(Rows) ? result.own_session! : (child.close; result)
    rescue Exception
      child&.close
      raise
    end
    def shared_cursor_pin(cursor, session)
      @shared_mutex.synchronize do
        @pins[cursor.object_id] = (@pins[session.object_id] || [@logical_version, shared_file_txid]).dup
      end
    end
  end
  Database.prepend(CursorDatabase)

  module SessionCursors
    def register_cursor(cursor) = ((@cursors ||= {})[cursor.object_id] = cursor)
    def unregister_cursor(cursor) = @cursors&.delete(cursor.object_id)
    def query_prepared(statement, *params)
      ast = statement.ast
      return execute_prepared(statement, *params) if statement_writes?(ast) || !%i[select values setop set_operation with].include?(ast[:kind])
      raise Error.new('55000', 'session is closed') if @session_closed
      raise Error.new('25P02', 'transaction is aborted') if @engine.instance_variable_get(:@failed)
      @database.shared_open!
      transient_pin = !@engine.transaction_open? && !@pinned
      @database.shared_capture(self, pin: true, read: true) if transient_pin
      params = params.map { |value| HostValues.copy(value) }
      PointRows.build(self, statement, params) || Rows.new(self, statement, params)
    rescue Error
      @engine.instance_variable_set(:@failed, true) if @engine.transaction_open? && $!.sqlstate != '25001'
      raise
    ensure
      @database.shared_unpin(self) if transient_pin
    end
    def execute(sql, *params)
      execute_prepared(prepare(sql), *params)
    rescue Error
      @engine.instance_variable_set(:@failed, true) if @engine.transaction_open? && $!.sqlstate != '25001'
      raise
    end
    def close
      @cursors&.values&.dup&.each(&:close)
      super
    end
  end
  # Capture the eager method before prepending; bind_call then cannot recurse
  # through the lazy public dispatch.
  SessionCursors::EAGER_QUERY = Session.instance_method(:query_prepared)
  module SessionCursors
    def execute_prepared(statement, *params)
      ended = @engine.transaction_open? && %i[commit rollback].include?(statement.ast[:kind])
      EAGER_QUERY.bind_call(self, statement, *params)
    ensure
      @cursors&.values&.dup&.each { |cursor| cursor.close if cursor.block_bound? } if ended && !@engine.transaction_open?
    end
  end
  Session.prepend(SessionCursors)
end

require_relative 'point_cursor'
