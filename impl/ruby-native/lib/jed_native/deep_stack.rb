# frozen_string_literal: true

module JedNative
  # SQL's nesting limit is independent of the host VM stack. Resolver layers
  # can use many Ruby frames for one SQL node, so long legal expressions resume
  # on another VM stack while retaining the exact recursive evaluation order.
  module DeepStack
    STACK_SLICE = 24
    def compile(node, columns)
      depth = @native_compile_depth || 0
      if depth >= STACK_SLICE
        @native_deep_compilation = true
        @native_compile_depth = 0
        return Fiber.new { compile(node, columns) }.resume
      end
      @native_compile_depth = depth + 1
      expression = super
      expression.instance_variable_set(:@native_deep_evaluation, true) if depth.zero? && @native_deep_compilation
      expression
    ensure
      @native_compile_depth = depth
      @native_deep_compilation = false if depth.zero?
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
