# frozen_string_literal: true

module JedNative
  # A column-only integer-key read needs a retained table root and its ordinary
  # point operator, not a copy of every executor subsystem. The general Rows
  # cursor still owns expressions, transactions, and resource-limited reads.
  module PointCursorPlanning
    PointCursorPlan = Struct.new(:table, :binding, :read, :names, :types, :ordinals,
      :labels, :schema_types, :environment, :timezone, :collation, keyword_init: true)

    def point_cursor_plan(statement)
      return if transaction_open? || @max_cost.positive? || @lifetime_max_cost.positive? ||
                @max_query_memory_bytes.positive? || @explain_profile_frames
      return unless @outer.empty? && @ctes.empty?
      return unless statement[:kind] == :select
      source = statement[:from]&.first
      return unless source && source[:kind] == :table && !source[:database]
      stored = @tables[source[:name]]
      return unless stored && !stored[:temporary] && !stored[:database]
      @native_point_cursor_plans ||= {}.compare_by_identity
      cached = @native_point_cursor_plans[statement]
      if cached && cached.table.equal?(stored) && cached.schema_types.equal?(@types) &&
         cached.environment.equal?(@environment) && cached.timezone == timezone && cached.collation == default_collation
        return cached if cached.read
        return nil
      end

      plan = PointCursorPlan.new(table: stored, schema_types: @types, environment: @environment,
                                 timezone: timezone, collation: default_collation)
      if point_cursor_syntax?(statement)
        ast, binding, metadata = cursor_metadata(statement)
        # Only the integer predicate can bind parameters in this shape. The
        # nil placeholders allow resolution without reading or coercing a host
        # value; actual bindings are validated below before cursor admission.
        previous_params, previous_types = @params, @native_binding_types
        begin
          @params, @native_binding_types = Array.new(binding.count), binding.types
          read = resolved_read_plan(ast, stored)
        ensure
          @params, @native_binding_types = previous_params, previous_types
        end
        if read.point && binding.types.all? { |type| Types::INTEGER_BITS.key?(type) } &&
           read.expressions.all? { |expression| expression.kind == :column }
          plan.binding, plan.read = binding, read
          plan.names = metadata.map { |column| column[:name].dup.freeze }.freeze
          plan.types = metadata.map { |column| column[:type].dup.freeze }.freeze
          plan.ordinals = read.expressions.map(&:value).freeze
          plan.labels = read.expressions.map { |expression| expression.instance_variable_get(:@native_collation) }.freeze
        end
      end
      @native_point_cursor_plans.shift if @native_point_cursor_plans.length >= 64
      @native_point_cursor_plans[statement] = plan
      plan if plan.read
    end

    private

    def point_cursor_syntax?(statement)
      return false unless statement[:from].length == 1 && streaming_select_syntax?(statement)
      return false if statement[:into] || statement[:distinct] || statement[:distinct_on] || statement[:limit] || statement[:offset] ||
                      !(statement[:order_by] || []).empty?
      return false unless statement[:columns].all? { |item| %i[column star].include?(item[:expr][:kind]) }
      predicate = statement[:where]
      return false unless predicate && predicate[:kind] == :binary && predicate[:op] == '='
      left, right = predicate[:left], predicate[:right]
      left, right = right, left if right[:kind] == :column
      return false unless left[:kind] == :column
      right[:kind] == :param || right[:kind] == :literal && (right[:value].nil? || right[:value].is_a?(Integer))
    end
  end
  Engine.prepend(PointCursorPlanning)

  class PointRows < Rows
    def self.build(session, statement, params)
      engine = session.engine
      plan = engine.point_cursor_plan(statement.ast)
      return unless plan
      binding = plan.binding
      raise Error.new('42601', 'bind parameter count mismatch') unless params.length == binding.count
      bound = binding.count.zero? ? params : binding.bind(params)
      engine.send(:authorize_node, statement.ast, {}, true, []) unless engine.unrestricted_capabilities?
      expression = plan.read.point.expression
      key = expression.kind == :param ? bound[expression.value] : expression.value
      new(session, plan, key)
    end

    def initialize(session, plan, key)
      @session, @database, @plan, @key = session, session.database, plan, key
      @meter = session.engine
      @columns, @column_types = plan.names.map(&:dup), plan.types.map(&:dup)
      @service_roles = session.engine.instance_variable_get(:@service_roles)&.dup
      @cost = 0
      @closed = @started = @block_bound = @collecting = false
      @database.shared_cursor_pin(self, session)
      session.register_cursor(self)
    end

    def cost = @cost
    def close = finish

    def finish
      return if @closed
      @closed = true
      begin
        @database.shared_unpin(self)
      ensure
        @session.unregister_cursor(self)
        @session.close if @owns_session
      end
      nil
    ensure
      @plan = @key = @service_roles = @session = @database = @meter = nil if @closed
    end

    private

    def charge(amount)
      @cost += amount
      @meter.instance_variable_set(:@lifetime_cost, @meter.lifetime_cost + amount)
    end

    def pull
      return nil if @closed
      return finish if @started
      @started = true
      # Cached pages must not conceal a poisoned/closed file or a page-service
      # role change. This is the same admission guard as Engine#run.
      @service_roles&.each do |storage, generation|
        storage.send(:ensure_open)
        raise Error.new('08006', 'transaction belongs to an earlier page-service role') unless storage.service_generation == generation
      end
      point = @plan.read.point
      return finish if @key.nil? || !@key.between?(point.minimum, point.maximum)
      charge(point.pages) unless point.pages.zero?
      row = point.index[@key]
      return finish unless row
      stored = @plan.table
      ordinal = row.is_a?(PagedRow) ? row : stored[:native_row_positions]&.[](row.object_id)
      if ordinal && stored[:native_primary_metadata]
        selected = @plan.read.scan_columns[stored[:name]]
        units = if row.is_a?(PagedRow) && stored[:rows].is_a?(PagedRows)
                  row.storage_leaf.units(row.storage_slot, selected)
                else
                  @meter.primary_scan_units(stored, [ordinal], selected)
                end
        payload_cost = units.sum
        charge(payload_cost) unless payload_cost.zero?
      end
      charge(1) # storage_row_read
      charge(1) # the original integer equality residual
      return finish unless row[stored[:native_primary_ordinal]] == @key
      values = Array.new(@plan.ordinals.length) do |index|
        column = @plan.ordinals[index]
        HostValues.copy(CollationRules.wrap(row[column], @plan.labels[index]))
      end
      charge(1) # row_produced, after projection/decode
      Row.new(values, @plan.names)
    rescue Exception
      finish
      raise
    end
  end
end
