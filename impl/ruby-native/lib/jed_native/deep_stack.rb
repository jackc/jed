# frozen_string_literal: true

module JedNative
  # SQL's nesting limit is independent of the host VM stack. Resolver layers
  # can use many Ruby frames for one SQL node, so long legal expressions resume
  # on another VM stack while retaining the exact recursive evaluation order.
  module DeepStack
    STACK_SLICE = 12
    # Relation wrappers consume more frames than scalar nodes. Only slice the
    # materializing source boundary: the root SELECT must yield directly to
    # its cursor's Fiber, and is deliberately not moved onto another stack.
    SOURCE_STACK_SLICE = 4
    def source_relation(source)
      depth = @native_source_depth || 0
      if depth >= SOURCE_STACK_SLICE
        @native_source_depth = 0
        return Fiber.new { source_relation(source) }.resume
      end
      @native_source_depth = depth + 1
      super
    ensure
      @native_source_depth = depth
    end

    def compile(node, columns)
      depth = @native_compile_depth || 0
      if depth >= STACK_SLICE
        @native_compile_depth = 0
        expression = Fiber.new { compile(node, columns) }.resume
        # The child stack's depth-zero ensure resets the flag. Propagate its
        # need for sliced evaluation back to the complete outer expression.
        @native_deep_compilation = true
        return expression
      end
      @native_compile_depth = depth + 1
      expression = super
      expression.instance_variable_set(:@native_deep_evaluation, true) if depth.zero? && @native_deep_compilation
      expression
    ensure
      @native_compile_depth = depth
      @native_deep_compilation = false if depth.zero?
    end

    def constant_expression?(expression)
      depth = @native_constant_depth || 0
      if depth >= STACK_SLICE
        @native_constant_depth = 0
        return Fiber.new { constant_expression?(expression) }.resume
      end
      @native_constant_depth = depth + 1
      super
    ensure
      @native_constant_depth = depth
    end

    def evaluate(expression, row, group = nil)
      return super unless @native_evaluation_depth || expression.instance_variable_get(:@native_deep_evaluation)
      depth = @native_evaluation_depth
      if depth && depth >= STACK_SLICE
        @native_evaluation_depth = 0
        return Fiber.new { evaluate(expression, row, group) }.resume
      end
      @native_evaluation_depth = (depth || 0) + 1
      begin
        super
      ensure
        @native_evaluation_depth = depth
      end
    ensure
      @native_evaluation_depth = depth if defined?(depth) && depth && depth >= STACK_SLICE
    end
  end
  Engine.prepend(DeepStack)
end
