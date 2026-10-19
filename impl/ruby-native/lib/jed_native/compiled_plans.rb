# frozen_string_literal: true

module JedNative
  # Generated methods belong to the existing bounded resolved-plan caches.
  # Compile only reused plans, after contextual resolution and filter marking.
  # Execution never rescues a compiled failure to retry in the interpreter.
  module CompiledPlans
    MAX_PLAN_SOURCE_BYTES = 65_536
    Kernels = Struct.new(:predicate, :projection, :keys, :pipeline, keyword_init: true)
    State = Struct.new(:uses, :kernels, :attempted, :source_bytes, keyword_init: true)
    MAX_ARGUMENT_CACHE_ENTRIES = 64

    def native_compiled_kernels(plan, predicate, projections, keys = nil, pipeline: nil, offset: false, limit: false)
      return if @native_compilation_disabled
      state = plan.instance_variable_get(:@native_compiled_state)
      unless state
        state = State.new(uses: 0, kernels: Kernels.new, attempted: {}, source_bytes: 0)
        plan.instance_variable_set(:@native_compiled_state, state)
      end
      state.uses += 1 if state.uses < 2
      return if state.uses < 2 && !@native_compilation_immediate

      if pipeline && !@native_pipeline_disabled
        unless state.attempted[:pipeline]
          state.attempted[:pipeline] = true
          native_admit_kernel(state, :pipeline,
            CompiledPipeline.build(predicate, projections, types: @types, mode: pipeline, offset: offset, limit: limit))
        end
        return state.kernels if state.kernels.pipeline
      end

      unless state.attempted[:predicate]
        state.attempted[:predicate] = true
        if predicate && !%i[literal column param].include?(predicate.kind)
          native_admit_kernel(state, :predicate, CompiledExpression.build(predicate, types: @types))
        end
      end
      unless state.attempted[:projection]
        state.attempted[:projection] = true
        native_admit_kernel(state, :projection, CompiledExpression.build_many(projections, types: @types))
      end
      if keys && !state.attempted[:keys]
        state.attempted[:keys] = true
        native_admit_kernel(state, :keys, CompiledExpression.build_many(keys, types: @types))
      end
      state.kernels
    end

    # Aggregate expressions are resolved afresh by the general executor. Cache
    # only completely specialized argument programs, by their exact source AND
    # data pool. This includes every literal/type/offset and never retains a
    # folded subquery or host expression under an AST identity alone.
    def native_compiled_aggregate_arguments(expression)
      return if @native_compilation_disabled
      return expression.instance_variable_get(:@native_argument_kernel) if expression.instance_variable_defined?(:@native_argument_kernel)
      arguments = expression.args
      return if arguments.all? { |argument| %i[literal column param star].include?(argument.kind) }
      program = CompiledExpression.prepare_many(arguments, types: @types)
      unless program
        expression.instance_variable_set(:@native_argument_kernel, nil)
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
      expression.instance_variable_set(:@native_argument_kernel, kernel)
      kernel
    end

    private

    def native_admit_kernel(state, slot, kernel)
      return unless kernel && state.source_bytes + kernel.source_bytes <= MAX_PLAN_SOURCE_BYTES
      state.source_bytes += kernel.source_bytes
      state.kernels[slot] = kernel
    end
  end
  Engine.prepend(CompiledPlans)
end
