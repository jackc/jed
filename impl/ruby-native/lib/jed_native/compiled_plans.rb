# frozen_string_literal: true

module JedNative
  # Generated methods belong to the existing bounded resolved-plan caches.
  # Compile only reused plans, after contextual resolution and filter marking.
  # Execution never rescues a compiled failure to retry in the interpreter.
  module CompiledPlans
    MAX_PLAN_SOURCE_BYTES = 65_536
    Kernels = Struct.new(:predicate, :projection, :keys, :pipeline, :ordering, keyword_init: true)
    State = Struct.new(:uses, :kernels, :attempted, :source_bytes, keyword_init: true)
    MAX_ARGUMENT_CACHE_ENTRIES = 64
    MAX_AGGREGATE_CACHE_ENTRIES = 64
    MAX_RELATION_CACHE_ENTRIES = 64
    MAX_JOIN_PREDICATE_CACHE_ENTRIES = 64
    MIN_JOIN_PREDICATE_CANDIDATES = 8

    def native_compiled_kernels(plan, predicate, projections, keys = nil, pipeline: nil, offset: false, limit: false, orders: nil)
      return if @native_compilation_disabled
      text = !@native_text_compilation_disabled
      slot = text ? :@native_compiled_state : :@native_compiled_state_without_text
      state = plan.instance_variable_get(slot)
      unless state
        state = State.new(uses: 0, kernels: Kernels.new, attempted: {}, source_bytes: 0)
        plan.instance_variable_set(slot, state)
      end
      state.uses += 1 if state.uses < 2
      return if state.uses < 2 && !@native_compilation_immediate

      if pipeline && !@native_pipeline_disabled
        unless state.attempted[:pipeline]
          state.attempted[:pipeline] = true
          native_admit_kernel(state, :pipeline,
            CompiledPipeline.build(predicate, projections, types: @types, mode: pipeline, offset: offset, limit: limit, text: text))
        end
        return state.kernels if state.kernels.pipeline
      end

      unless state.attempted[:predicate]
        state.attempted[:predicate] = true
        if predicate && !%i[literal column param].include?(predicate.kind)
          native_admit_kernel(state, :predicate, CompiledExpression.build(predicate, types: @types, text: text))
        end
      end
      unless state.attempted[:projection]
        state.attempted[:projection] = true
        native_admit_kernel(state, :projection, CompiledExpression.build_many(projections, types: @types, text: text))
      end
      if keys && !state.attempted[:keys]
        state.attempted[:keys] = true
        native_admit_kernel(state, :keys, CompiledExpression.build_many(keys, types: @types, text: text))
      end
      if orders && !@native_ordering_disabled && !state.attempted[:ordering]
        state.attempted[:ordering] = true
        native_admit_kernel(state, :ordering, CompiledOrdering.build(keys, orders, types: @types))
      end
      state.kernels
    end

    # Join predicates can reuse resolved expressions, but those expressions
    # remain mutable. Prepare their current complete source and immutable pool
    # on every acquisition; only generated kernels survive in this bounded LRU.
    # No predicate, row, binding, plan or execution owner is retained.
    def native_compiled_join_predicate(expression)
      return if @native_compilation_disabled || @native_join_compilation_disabled
      return unless expression.is_a?(Expression) && !%i[literal column param].include?(expression.kind)
      program = CompiledExpression.prepare(expression, types: @types, text: !@native_text_compilation_disabled)
      return unless program
      @native_compiled_join_predicates ||= {}
      key = [program.source, program.pool].freeze
      if @native_compiled_join_predicates.key?(key)
        kernel = @native_compiled_join_predicates.delete(key)
        kernel ||= program.materialize
        @native_compiled_join_predicates[key] = kernel
      else
        @native_compiled_join_predicates.shift if @native_compiled_join_predicates.length >= MAX_JOIN_PREDICATE_CACHE_ENTRIES
        kernel = program.materialize if @native_compilation_immediate
        @native_compiled_join_predicates[key] = kernel
      end
      kernel
    end

    # Aggregate expressions are resolved afresh by the general executor. Cache
    # only completely specialized argument programs, by their exact source AND
    # data pool. This includes every literal/type/offset and never retains a
    # folded subquery or host expression under an AST identity alone.
    def native_compiled_aggregate_arguments(expression)
      return if @native_compilation_disabled
      text = !@native_text_compilation_disabled
      slot = text ? :@native_argument_kernel : :@native_argument_kernel_without_text
      return expression.instance_variable_get(slot) if expression.instance_variable_defined?(slot)
      arguments = expression.args
      return if arguments.all? { |argument| %i[literal column param star].include?(argument.kind) }
      program = CompiledExpression.prepare_many(arguments, types: @types, text: text)
      unless program
        expression.instance_variable_set(slot, nil)
        return
      end
      @native_compiled_arguments ||= {}
      key = [program.source, program.pool].freeze
      if @native_compiled_arguments.key?(key)
        kernel = @native_compiled_arguments.delete(key)
        kernel ||= program.materialize
        @native_compiled_arguments[key] = kernel
      else
        @native_compiled_arguments.shift if @native_compiled_arguments.length >= MAX_ARGUMENT_CACHE_ENTRIES
        kernel = program.materialize if @native_compilation_immediate
        @native_compiled_arguments[key] = kernel
      end
      expression.instance_variable_set(slot, kernel)
      kernel
    end

    # The general executor resolves aggregate expressions afresh. Reuse only
    # identical bounded programs and immutable pools, never AST identities or
    # expression objects containing folded subqueries. The first resolution
    # records admission; a later resolution may materialize the method.
    def native_compiled_aggregate(expression)
      return if @storage || @native_compilation_disabled || @native_aggregate_disabled
      text = !@native_text_compilation_disabled
      slot = text ? :@native_aggregate_kernel : :@native_aggregate_kernel_without_text
      return expression.instance_variable_get(slot) if expression.instance_variable_defined?(slot)
      program = CompiledAggregate.prepare(expression, types: @types, text: text)
      unless program
        expression.instance_variable_set(slot, nil)
        return
      end
      @native_compiled_aggregates ||= {}
      key = [program.source, program.pool].freeze
      if @native_compiled_aggregates.key?(key)
        kernel = @native_compiled_aggregates.delete(key)
        kernel ||= program.materialize
        @native_compiled_aggregates[key] = kernel
      else
        @native_compiled_aggregates.shift if @native_compiled_aggregates.length >= MAX_AGGREGATE_CACHE_ENTRIES
        kernel = program.materialize if @native_compilation_immediate
        @native_compiled_aggregates[key] = kernel
      end
      expression.instance_variable_set(slot, kernel)
      kernel
    end

    # General relation phases are resolved on each execution, like aggregates.
    # Their source-and-pool cache contains no AST, row, or accounting owner.
    def native_compiled_relation_filter(predicate, memory: false)
      return if @storage || @native_compilation_disabled || @native_relation_disabled || !predicate
      text = !@native_text_compilation_disabled
      slot = if text
        memory ? :@native_relation_memory_filter_kernel : :@native_relation_filter_kernel
      else
        memory ? :@native_relation_memory_filter_kernel_without_text : :@native_relation_filter_kernel_without_text
      end
      return predicate.instance_variable_get(slot) if predicate.instance_variable_defined?(slot)
      program = CompiledRelation.prepare_filter(predicate, types: @types, memory: memory, text: text)
      kernel = native_relation_program_kernel(program)
      predicate.instance_variable_set(slot, kernel)
      kernel
    end

    def native_compiled_relation_keys(expressions)
      return if @storage || @native_compilation_disabled || @native_relation_disabled
      text = !@native_text_compilation_disabled
      slot = text ? :@native_relation_keys_kernel : :@native_relation_keys_kernel_without_text
      return expressions.instance_variable_get(slot) if expressions.instance_variable_defined?(slot)
      program = CompiledRelation.prepare_keys(expressions, types: @types, text: text)
      kernel = native_relation_program_kernel(program)
      expressions.instance_variable_set(slot, kernel)
      kernel
    end

    private

    def native_relation_program_kernel(program)
      return unless program
      @native_compiled_relations ||= {}
      key = [program.source, program.pool].freeze
      if @native_compiled_relations.key?(key)
        kernel = @native_compiled_relations.delete(key)
        kernel ||= program.materialize
        @native_compiled_relations[key] = kernel
      else
        @native_compiled_relations.shift if @native_compiled_relations.length >= MAX_RELATION_CACHE_ENTRIES
        kernel = program.materialize if @native_compilation_immediate
        @native_compiled_relations[key] = kernel
      end
      kernel
    end

    def native_admit_kernel(state, slot, kernel)
      return unless kernel && state.source_bytes + kernel.source_bytes <= MAX_PLAN_SOURCE_BYTES
      state.source_bytes += kernel.source_bytes
      state.kernels[slot] = kernel
    end
  end
  Engine.prepend(CompiledPlans)
end
