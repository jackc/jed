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
      validate_cte_structure(ast, true)
      @native_cursor_metadata ||= {}.compare_by_identity
      entry = @native_cursor_metadata[ast]
      unless entry && parameter_binding_schema_current?(entry[0]) &&
             entry[1][0] == @environment && entry[1][1] == timezone && entry[1][2] == default_collation
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
        entry = @native_cursor_metadata[ast] = [parameter_binding_schema,
          [@environment, timezone, default_collation], [ast, binding, columns].freeze]
      end
      entry[2]
    end

    def select(statement)
      @cursor_select_depth = (@cursor_select_depth || 0) + 1 if @native_cursor
      root = @native_cursor && @cursor_select_depth == 1
      previous_buffered = @cursor_buffered_output
      # WITH owns its final result until the body has completed. Preserve that
      # buffered drive, including its output reservations, before yielding it.
      @cursor_buffered_output = true if root && statement[:with]
      result = super
      if root && !result.rows.is_a?(CursorOutput)
        cursor_buffered_result(result)
      else
        result
      end
    ensure
      @cursor_buffered_output = previous_buffered
      @cursor_select_depth -= 1 if @native_cursor
    end
    def cursor_output? = @native_cursor && @cursor_select_depth == 1 && !@cursor_buffered_output
    def cursor_output(columns) = cursor_output? ? CursorOutput.new(self, columns) : []
    def cursor_yield(row, columns)
      @native_cursor_emitted = true
      @native_cursor.accept_row(row, columns)
    end
    def cursor_buffered_result(relation)
      rows = relation.rows
      owner = relation.instance_variable_get(:@native_memory_owner)
      mask = relation.instance_variable_get(:@native_memory_mask)
      output = CursorOutput.new(self, relation.columns)
      index = 0
      while index < rows.length
        row = rows[index]
        rows[index] = nil
        index += 1
        # A buffered row transfers out of the engine before a materializing
        # host collector admits its copy. Keep the statement account alive
        # across suspension, instead of yielding after Engine#run unwinds it.
        query_memory_release(owner, query_memory_row_bytes(row, relation.columns, mask)) if @query_memory && owner
        output << row
      end
      relation.rows = output
      relation
    ensure
      query_memory_release(owner) if @query_memory && owner
    end
    def cacheable_select?(statement)
      if cursor_output? && (!(statement[:from] || []).empty? || !(statement[:order_by] || []).empty?)
        return false unless columnar_cursor?(statement)
      end
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
    SHARED_CACHES = %i[@native_read_plans @native_select_plans @native_join_resolution_plans @native_compiled_arguments @native_compiled_aggregates @native_compiled_relations @native_cte_validation
                       @native_window_shapes @native_spill_shapes @native_stream_shapes].freeze
    REPLACED_STATE = (SharedDatabase::CATALOGS + %i[@native_binding_plans @savepoints
      @params @foreign_key_checks @native_query_shape_cache @subquery_hoists]).freeze
    # These maps carry no mutable execution state. Domain routing rechecks its
    # namespace qualifications on each use; the other two caches are consulted
    # only when constructing a cursor. Service roles are replaced wholesale at
    # capture, so retaining the old map preserves the cursor's admission guard.
    # Refresh provenance holds identity-only references, never mutable work.
    SHARED_REFERENCES = %i[@native_domain_routes @native_cursor_metadata
                           @native_point_cursor_plans @native_scalar_cursor_plans @service_roles
                           @native_refreshed_tables @native_refreshed_types].freeze
    COPY_OMISSIONS = (SHARED_CACHES + REPLACED_STATE + SHARED_REFERENCES).to_h { |name| [name, true] }.freeze

    def initialize(session, statement, params)
      @session, @database = session, session.database
      ast, binding, metadata = session.engine.cursor_metadata(statement.ast)
      raise Error.new('42601', 'bind parameter count mismatch') unless params.length == binding.count
      bound = binding.bind(params) unless binding.count.zero?
      prebound_types = binding.types if binding.reusable_bound_values?
      params = bound if prebound_types && bound
      SHARED_CACHES.each do |name|
        next if session.engine.instance_variable_defined?(name)
        cache = %i[@native_compiled_arguments @native_compiled_aggregates @native_compiled_relations].include?(name) ? {} : {}.compare_by_identity
        session.engine.instance_variable_set(name, cache)
      end
      @engine = session.engine.dup
      @engine.instance_variables.each do |name|
        next if COPY_OMISSIONS.key?(name)
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
      @engine.instance_variable_set(:@native_join_resolution_frame, nil)
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
          @engine.instance_variable_set(:@lifetime_cost, session.engine.lifetime_cost)
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
      ScalarRows.build(self, statement, params) || PointRows.build(self, statement, params) || Rows.new(self, statement, params)
    rescue Error
      @engine.instance_variable_set(:@failed, true) if @engine.transaction_open? && $!.sqlstate != '25001'
      raise
    ensure
      @database.shared_unpin(self) if transient_pin
    end
    def execute(sql, *params)
      execute_prepared(cached_statement(sql), *params)
    rescue Error
      @engine.instance_variable_set(:@failed, true) if @engine.transaction_open? && $!.sqlstate != '25001'
      raise
    end
    def close
      @cursors&.values&.dup&.each(&:close)
      @native_eager_cursor_seen&.clear
      super
    end
  end
  # Capture the eager method before prepending; bind_call then cannot recurse
  # through the lazy public dispatch.
  SessionCursors::EAGER_QUERY = Session.instance_method(:query_prepared)
  module SessionCursors
    def execute_prepared(statement, *params)
      ended = @engine.transaction_open? && %i[commit rollback].include?(statement.ast[:kind])
      if (cursor = eager_cursor(statement, params))
        execution = cursor.instance_variable_get(:@engine)
        begin
          # Fast cursors already detach values and metadata. Keep Result#rows
          # as ordinary arrays, matching the eager host API.
          rows = cursor.rows.map(&:to_a)
          return Result.new(columns: cursor.columns, column_types: cursor.column_types,
            rows: rows, cost: cursor.cost)
        ensure
          cursor.close
          @engine.instance_variable_set(:@cost, cursor.cost)
          @engine.instance_variable_set(:@scalar_bytes, execution ? execution.instance_variable_get(:@scalar_bytes) : 0)
          @engine.instance_variable_set(:@query_memory_peak, 0)
        end
      end
      EAGER_QUERY.bind_call(self, statement, *params)
    ensure
      @cursors&.values&.dup&.each { |cursor| cursor.close if cursor.block_bound? } if ended && !@engine.transaction_open?
    end

    private

    def eager_cursor(statement, params)
      ast = statement.ast
      return unless ast[:kind] == :select && !@engine.transaction_open? && !@engine.max_query_memory_bytes.positive?
      return if @session_closed || @engine.instance_variable_get(:@failed)
      # Fresh SQL already resolves once in the eager executor. Only repeat
      # uses can repay a cursor plan's additional metadata admission work.
      # This bounded session-local hint owns no bindings or catalog state.
      seen = (@native_eager_cursor_seen ||= {}.compare_by_identity)
      unless seen[ast]
        seen.shift if seen.length >= 64
        seen[ast] = true
        return
      end
      # Preserve Engine#run's admission error and its unchanged prior meter
      # when the session has exhausted its lifetime allowance.
      return if @engine.lifetime_max_cost.positive? && @engine.lifetime_cost >= @engine.lifetime_max_cost
      if (ast[:from] || []).empty?
        # Synchronous host callbacks can inspect the live eager meter and
        # change session budgets. Their original executor keeps those effects;
        # the lazy API independently retains its captured cursor semantics.
        return unless @engine.send(:scalar_cursor_syntax?, ast, host: false)
      else
        return unless @engine.send(:point_cursor_syntax?, ast)
      end
      @database.shared_open!
      transient_pin = !@pinned
      @database.shared_capture(self, pin: true, read: true) if transient_pin
      # The ordinary eager entry checks service health before resetting cost or
      # binding parameters. Keep that order ahead of cursor construction too.
      @engine.instance_variable_get(:@service_roles)&.each do |storage, generation|
        storage.send(:ensure_open)
        raise Error.new('08006', 'transaction belongs to an earlier page-service role') unless storage.service_generation == generation
      end
      copied = params.map { |value| HostValues.copy(value) }
      begin
        ScalarRows.build(self, statement, copied) || PointRows.build(self, statement, copied)
      rescue Error
        # No kernel or SQL work has run. The eager path owns the precedence of
        # metadata, binding and capability failures and resets its own meter.
        nil
      end
    ensure
      @database.shared_unpin(self) if transient_pin
    end
  end
  Session.prepend(SessionCursors)
end

require_relative 'point_cursor'
require_relative 'scalar_cursor'
