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
      return false if cursor_output?
      super
    end
    def streaming_select_shape?(statement)
      return super unless cursor_output? && !(statement[:limit] || statement[:offset]) && (statement[:order_by] || []).empty?
      super(statement.merge(offset: { kind: :literal, value: 0, type: 'i64' }))
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

    def initialize(session, statement, params)
      @session, @database = session, session.database
      @engine = session.engine.dup
      @engine.instance_variables.each do |name|
        value = @engine.instance_variable_get(name)
        @engine.instance_variable_set(name, value.dup) if value.is_a?(Hash) || value.is_a?(Array)
      end
      SharedDatabase::CATALOGS.each do |name|
        values = session.engine.instance_variable_get(name)
        @engine.instance_variable_set(name, name == :@types ? values.dup : values.transform_values(&:dup))
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
      ast = @engine.send(:route_domains, statement.ast)
      binding = ParameterBindings.new(@engine, ast)
      binding.resolve
      raise Error.new('42601', 'bind parameter count mismatch') unless params.length == binding.count
      binding.bind(params) unless binding.count.zero?
      @engine.instance_variable_set(:@native_binding_types, binding.types)
      metadata = @engine.query_metadata(ast)[:columns]
      @engine.instance_variable_set(:@native_binding_types, nil)
      @columns = metadata.map { |column| column[:name].dup }
      @column_types = metadata.map { |column| column[:type].dup }
      @engine.send(:authorize_node, ast, {}, true, [])
      @closed, @collecting, @cached_rows = false, false, nil
      @block_bound = session.engine.transaction_open?
      @database.shared_cursor_pin(self, session)
      session.register_cursor(self)
      @fiber = Fiber.new do
        begin
          result = @engine.run(statement.ast, params)
          unless @engine.instance_variable_get(:@native_cursor_emitted)
            result.rows.each { |row| accept_row(row, result.columns.zip(result.column_types).map { |name, type| { name: name, type: type } }) }
          end
        rescue CursorClosed
          # Closing a read cursor discards unread output without failing its block.
        rescue Exception
          session.engine.instance_variable_set(:@failed, true) if @block_bound && session.engine.transaction_open?
          raise
        ensure
          finish
        end
        nil
      end
    end

    def own_session! = (@owns_session = true; self)
    def collecting? = @collecting
    def block_bound? = @block_bound
    def closed? = @closed
    def cost = @engine.cost
    def rows_affected = nil
    def accept_row(row, columns)
      if @collecting && @engine.instance_variable_get(:@query_memory)
        @collection_owner ||= @engine.query_memory_owner
        @engine.query_memory_add_row(@collection_owner, row, columns)
      end
      @columns = columns.map { |column| column[:name].dup }
      @column_types = columns.map { |column| column[:type].dup }
      Fiber.yield(Row.new(row.map { |value| HostValues.copy(value) }, @columns))
    end
    def next
      raise StopIteration if @closed
      @started = true
      row = @fiber.resume
      raise StopIteration unless row
      row
    rescue Exception
      finish unless @fiber.alive?
      raise
    end
    def each
      return enum_for(:each) unless block_given?
      begin
        loop { yield self.next }
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
      @database.shared_unpin(self)
      @session.unregister_cursor(self)
      @session.close if @owns_session
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
      @database.shared_capture(self, pin: true) if transient_pin
      Rows.new(self, statement, params.map { |value| HostValues.copy(value) })
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
