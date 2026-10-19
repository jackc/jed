# frozen_string_literal: true

module JedNative
  # Scalar SELECTs need an expression context, but no relational executor
  # snapshot or suspended Fiber. Resolution and evaluation still use the normal
  # compiler, evaluator, and meters, and start only on the first pull.
  module ScalarCursorPlanning
    TYPES = %w[i16 i32 i64 decimal f32 f64 boolean text unknown null].to_h { |type| [type, true] }.freeze
    OPERATORS = %w[+ - * / % ^ & | # << >> || = <> != < <= > >= and or is\ distinct\ from is\ not\ distinct\ from].to_h { |operator| [operator, true] }.freeze
    # Text length/position utilities also have stateless bytea overloads. Their
    # text form is safe here; bytea expression inputs still use ordinary Rows.
    FAMILIES = %w[integer decimal float boolean text bytea any anycompatible].freeze
    FUNCTIONS = FunctionCatalog::SIGNATURES.filter_map do |name, signatures|
      next if name.start_with?('regexp_') || %w[like ilike like_escape].include?(name)
      next unless FunctionCatalog::VOLATILITY[name]&.all? { |_, volatility| volatility == 'immutable' }
      next unless signatures.any? { |arguments, *| !arguments.include?('bytea') }
      next unless signatures.all? { |arguments, result, *| arguments.all? { |family| FAMILIES.include?(family) } && (TYPES.key?(result) || result == 'promoted') }
      name
    end.concat(%w[coalesce nullif greatest least lower upper]).to_h { |name| [name, true] }.freeze
    EMPTY_ARRAY = [].freeze
    EMPTY_HASH = {}.freeze
    ScalarCursorPlan = Struct.new(:ast, :binding, :metadata, :names, :types, :catalog_types, :general_projection, keyword_init: true)

    def scalar_cursor_plan(statement)
      return unless statement[:kind] == :select && (statement[:from] || []).empty?
      return if transaction_open? || @max_query_memory_bytes.positive? || @explain_profile_frames
      return unless @outer.empty? && @ctes.empty?
      ast, binding, metadata = cursor_metadata(statement)
      @native_scalar_cursor_plans ||= {}.compare_by_identity
      cached = @native_scalar_cursor_plans[ast]
      return cached[1] if cached && cached[0].equal?(metadata)

      plan = nil
      if scalar_cursor_syntax?(ast) && binding.types.all? { |type| TYPES.key?(type) && !@types.key?(type) } &&
         metadata.all? { |column| TYPES.key?(column[:type]) && !@types.key?(column[:type]) }
        general_projection = ast[:columns].any? { |item| !cacheable_expression?(item[:expr]) } ||
                             ast[:where] && !cacheable_expression?(ast[:where])
        # The general executor filters before compiling projections. Its
        # conditional shapes keep that route when WHERE is present, including
        # which error wins when both the filter and a projection are invalid.
        unless general_projection && ast[:where]
          plan = ScalarCursorPlan.new(ast: ast, binding: binding, metadata: metadata,
            names: metadata.map { |column| column[:name].dup.freeze }.freeze,
            types: metadata.map { |column| column[:type].dup.freeze }.freeze,
            catalog_types: @types.dup.freeze, general_projection: general_projection)
          @native_select_plans ||= {}.compare_by_identity
        end
      end
      @native_scalar_cursor_plans.shift if @native_scalar_cursor_plans.length >= 64
      @native_scalar_cursor_plans[ast] = [metadata, plan]
      plan
    end

    def initialize_scalar_cursor(cursor, session, plan, params)
      @types, @params = plan.catalog_types, params
      @native_cursor, @cursor_session = cursor, session
      @native_binding_types, @native_binding_active = plan.binding.types, true
      @cost = @scalar_bytes = 0
      @transaction, @dirty, @closed = nil, false, false
      @outer, @ctes = EMPTY_ARRAY, EMPTY_HASH
      @query_memory = @query_memory_contexts = @query_memory_aggregate_owner = nil
      @native_compile_depth = @native_evaluation_depth = @native_literal_container = nil
      @native_deep_compilation = false
      @host_batch_source_charges = @host_batch_stack = nil
      @aggregate_group_states = @aggregate_cached_results = nil
      @regex_dispatch_active = false
      @regex_charged = @regex_programs = nil
      @statement_clock, @uuid_counter = nil, 0
      self
    end

    def scalar_cursor_values(cursor_plan)
      # Engine#run checks this before compilation, even for a predicate that
      # suppresses all output without charging any work.
      @lifetime_cost = @cursor_session.engine.lifetime_cost
      if @lifetime_max_cost.positive? && @lifetime_cost >= @lifetime_max_cost
        raise Error.new('54P02', 'session cost limit exceeded')
      end
      statement = cursor_plan.ast
      types = @native_binding_types
      context = [@types, @environment, timezone, default_collation]
      plan = @native_select_plans[statement]
      unless plan && plan.table.nil? && plan.parameter_types == types && plan.context == context
        plan = resolve_select_plan(statement, nil, nil, types)
        plan.context = context
        @native_select_plans.shift if @native_select_plans.length >= PreparedSelect::CACHE_ENTRIES
        @native_select_plans[statement] = plan
      end
      plan.parameters.each do |parameter|
        cast_value(@params[parameter.value], parameter.type) if parameter.extra&.dig(:context_type)
      end
      kernels = native_compiled_kernels(plan, plan.predicate, plan.projections) unless cursor_plan.general_projection
      return nil if where_contradiction?(plan.predicate)
      return nil if plan.predicate && (kernels&.predicate ? kernels.predicate.call(self, EMPTY_ARRAY, @params) : evaluate(plan.predicate, EMPTY_ARRAY)) != true
      if cursor_plan.general_projection
        # General deferred projections reserve the output row before evaluating
        # its expressions. Their zero-work checkpoints and error cost prefixes
        # differ from the prepared expression lane, which charges afterward.
        charge(0)
        @cost += 1
        @lifetime_cost += 1
        @cursor_session.engine.instance_variable_set(:@lifetime_cost, @lifetime_cost)
        values = Array.new(plan.projections.length) do |index|
          charge(0)
          evaluate(plan.projections[index], EMPTY_ARRAY)
        end
      else
        values = kernels&.projection ? kernels.projection.call(self, EMPTY_ARRAY, @params) :
          Array.new(plan.projections.length) { |index| evaluate(plan.projections[index], EMPTY_ARRAY) }
        charge # row_produced, after all projection work
      end
      values
    end

    private

    def scalar_cursor_syntax?(statement, host: true)
      return false if statement[:with] || statement[:into] || statement[:distinct] || statement[:distinct_on] ||
                      statement[:having] || statement[:limit] || statement[:offset]
      return false unless (statement[:joins] || []).empty? && (statement[:group_by] || []).empty? &&
                          (statement[:windows] || {}).empty? && (statement[:order_by] || []).empty?
      statement[:columns].all? { |item| scalar_cursor_expression?(item[:expr], host: host) } &&
        (!statement[:where] || scalar_cursor_expression?(statement[:where], host: host))
    end

    def scalar_cursor_expression?(node, host: true)
      case node[:kind]
      when :literal
        value = node[:value]
        TYPES.key?(node[:type] || Types.infer(value)) &&
          (value.nil? || value == true || value == false || value.is_a?(Integer) || value.is_a?(Float) || value.is_a?(Decimal) || value.is_a?(String))
      when :param then true
      when :cast
        type = Types.normalize(node[:type])
        return false unless TYPES.key?(type) && !@types.key?(type)
        child = node[:expr]
        child[:kind] == :literal && child[:value].is_a?(String) || scalar_cursor_expression?(child, host: host)
      when :unary then scalar_cursor_expression?(node[:expr] || node[:operand], host: host)
      when :binary
        OPERATORS.key?(node[:op]) && scalar_cursor_expression?(node[:left], host: host) && scalar_cursor_expression?(node[:right], host: host)
      when :case
        (!node[:operand] || scalar_cursor_expression?(node[:operand], host: host)) &&
          node[:branches].all? do |branch|
            condition, value = branch.is_a?(Array) ? branch : [branch[:when], branch[:then]]
            scalar_cursor_expression?(condition, host: host) && scalar_cursor_expression?(value, host: host)
          end && (!node[:else] || scalar_cursor_expression?(node[:else], host: host))
      when :between
        scalar_cursor_expression?(node[:expr], host: host) && scalar_cursor_expression?(node[:low] || node[:lower], host: host) &&
          scalar_cursor_expression?(node[:high] || node[:upper], host: host)
      when :call
        return false if PreparedSelect::STATEFUL_FUNCTIONS.include?(node[:name])
        callable = if host_function_name?(node[:name])
          return false unless host
          return false if builtin_function_name?(node[:name]) && !FUNCTIONS.key?(node[:name])
          # Host callbacks are never folded. Their frozen signature and scalar
          # result need no relational executor state; evaluation still goes
          # through HostFunctions at first pull. Batched callbacks retain the
          # general executor's speculative prefetch and replay boundaries.
          host_functions_named(node[:name]).all? do |function|
            function.kernel && TYPES.key?(function.result) && function.args.all? { |type| TYPES.key?(type) }
          end
        else
          FUNCTIONS.key?(node[:name])
        end
        callable && !node[:over] && !node[:filter] &&
          !node[:within_group] && !node[:distinct] && !node[:variadic] && (node[:order_by] || []).empty? &&
          (node[:arg_names] || []).empty? && !Functions.aggregate?(node[:name]) &&
          (node[:args] || []).all? { |argument| scalar_cursor_expression?(argument, host: host) }
      else false
      end
    end
  end
  Engine.prepend(ScalarCursorPlanning)

  class ScalarRows < Rows
    def self.build(session, statement, params)
      engine = session.engine
      plan = engine.scalar_cursor_plan(statement.ast)
      return unless plan
      binding = plan.binding
      raise Error.new('42601', 'bind parameter count mismatch') unless params.length == binding.count
      bound = binding.count.zero? ? params : binding.bind(params)
      engine.send(:authorize_node, statement.ast, {}, true, []) unless engine.unrestricted_capabilities?
      new(session, plan, bound)
    end

    def initialize(session, plan, params)
      @session, @database, @plan = session, session.database, plan
      @engine = session.engine.dup.initialize_scalar_cursor(self, session, plan, params)
      @columns, @column_types = plan.names.map(&:dup), plan.types.map(&:dup)
      @service_roles = session.engine.instance_variable_get(:@service_roles)&.dup
      @closed = @started = @block_bound = @collecting = false
      @database.shared_cursor_pin(self, session)
      session.register_cursor(self)
    end

    def close = finish

    def finish
      super
      nil
    ensure
      @plan = @service_roles = nil if @closed
    end

    private

    def pull
      return nil if @closed
      return finish if @started
      @started = true
      @service_roles&.each do |storage, generation|
        storage.send(:ensure_open)
        raise Error.new('08006', 'transaction belongs to an earlier page-service role') unless storage.service_generation == generation
      end
      values = @engine.scalar_cursor_values(@plan)
      return finish unless values
      @columns, @column_types = @plan.names.map(&:dup), @plan.types.map(&:dup)
      Row.new(values.map { |value| HostValues.copy(value) }, @plan.names)
    rescue Exception
      finish
      raise
    end
  end
end
