# frozen_string_literal: true

module JedNative
  # Collect into one fresh result instead of building and concatenating arrays
  # at every node. Expression identity, duplicates and traversal order survive.
  module ExpressionWalks
    EMPTY_ARGS = [].freeze

    private

    def expression_nodes(expression)
      return super if @native_expression_walks_disabled ||
        expression.is_a?(Array) && !expression.instance_of?(Array) ||
        expression.is_a?(Expression) && !expression.instance_of?(Expression)
      nodes = []
      append_expression_nodes(expression, nodes)
      nodes
    end

    def append_expression_nodes(expression, nodes)
      if expression.instance_of?(Expression)
        nodes << expression
        args = expression.args
        if args.instance_of?(Array)
          args.each { |argument| append_expression_nodes(argument, nodes) }
        elsif args
          nodes.concat(args.flat_map { |argument| expression_nodes(argument) })
        end
      elsif expression.instance_of?(Array)
        expression.each { |part| append_expression_nodes(part, nodes) }
      elsif expression.is_a?(Expression) || expression.is_a?(Array)
        nodes.concat(expression_nodes(expression))
      end
    end

    def conjunctions(expression)
      return super if @native_expression_walks_disabled || !expression.instance_of?(Expression)
      return [expression] unless expression.kind == :binary && expression.value == 'and'
      terms = []
      append_conjunctions(expression, terms)
      terms
    end

    def append_conjunctions(expression, terms)
      if !expression.instance_of?(Expression)
        terms.concat(conjunctions(expression))
      elsif expression.kind == :binary && expression.value == 'and'
        args = expression.args
        if args.instance_of?(Array)
          args.each { |argument| append_conjunctions(argument, terms) }
        else
          terms.concat(args.flat_map { |argument| conjunctions(argument) })
        end
      else
        terms << expression
      end
    end

    def same_key_expression?(left, right)
      return super if @native_expression_walks_disabled
      return false unless left.is_a?(Expression) && right.is_a?(Expression)
      return super unless left.instance_of?(Expression) && right.instance_of?(Expression)
      if left.kind == :column && right.kind == :column && left.value == right.value &&
         Types::INTEGER_BITS.key?(left.type) && Types::INTEGER_BITS.key?(right.type) &&
         (left.extra&.dig(:native_body_type) || right.extra&.dig(:native_body_type))
        return true
      end

      # Array#== compares these fields by identity or ==. In particular, a
      # shared NaN value is equal here even though NaN == NaN is false.
      left_kind = left.kind
      left_type = left.type
      left_value = left.value
      right_kind = right.kind
      right_type = right.type
      right_value = right.value
      return false unless (left_kind.equal?(right_kind) || left_kind == right_kind) &&
        (left_type.equal?(right_type) || left_type == right_type) &&
        (left_value.equal?(right_value) || left_value == right_value)
      return false if left_kind == :cast && left.extra&.dig(:type_mod) != right.extra&.dig(:type_mod)

      a = left.args || EMPTY_ARGS
      b = right.args || EMPTY_ARGS
      length = a.length
      return false unless length == b.length
      unless a.instance_of?(Array) && b.instance_of?(Array)
        return a.zip(b).all? { |x, y| x.is_a?(Expression) ? same_key_expression?(x, y) : x == y }
      end
      index = 0
      while index < length
        x = a[index]
        y = b[index]
        # CASE branch arrays retain ordinary Array/Struct equality. Only a
        # directly held Expression takes this helper's narrower comparison.
        return false unless x.is_a?(Expression) ? same_key_expression?(x, y) : x == y
        index += 1
      end
      true
    end
  end
  Engine.prepend(ExpressionWalks)
end
