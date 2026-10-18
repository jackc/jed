# frozen_string_literal: true

module JedNative
  # The bounded, conservative prover in indexes.md §9.1. It inspects resolved
  # expressions and never executes SQL while choosing an index or arbiter.
  class PredicateImplication
    COMPARISONS = %w[= <> < <= > >=].freeze
    FLIPPED = { '=' => '=', '<>' => '<>', '<' => '>', '<=' => '>=', '>' => '<', '>=' => '<=' }.freeze

    def initialize(engine)
      @engine, @remaining = engine, 10_000
    end

    def conjunction(terms, predicate)
      return false unless step
      predicate = expand(predicate)
      return conjunction(terms, predicate.args[0]) && conjunction(terms, predicate.args[1]) if logical?(predicate, 'and')
      return true if terms.any? { |term| clause(term, predicate) }
      logical?(predicate, 'or') && (conjunction(terms, predicate.args[0]) || conjunction(terms, predicate.args[1]))
    end

    private

    def step
      @remaining -= 1
      @remaining >= 0
    end

    def logical?(expression, operator)
      expression.kind == :binary && expression.value == operator
    end

    def equal?(left, right)
      @engine.send(:same_key_expression?, left, right) && ordering_collation(left) == ordering_collation(right)
    end

    def ordering_collation(expression)
      return nil unless expression.kind == :binary && %w[< <= > >=].include?(expression.value)
      label = expression.instance_variable_get(:@native_collation)
      label && [label.name, label.conflict, label.none]
    end

    def clause(left, right)
      return false unless step
      left, right = expand(left), expand(right)
      return true if equal?(left, right)
      return clause(left, right.args[0]) && clause(left, right.args[1]) if logical?(right, 'and')
      return clause(left.args[0], right) && clause(left.args[1], right) if logical?(left, 'or')
      return clause(left.args[0], right) || clause(left.args[1], right) if logical?(left, 'and')
      if left.kind == :quantified && left.extra[:op] == '=' && left.extra[:quantifier] != 'all'
        array = left.args[1]
        elements = if array.kind == :array
          array.args
        elsif array.kind == :literal && array.value.is_a?(ArrayValue) && array.value.dimensions.length <= 1
          array.value.map { |value| Expression.new(kind: :literal, type: array.type.delete_suffix('[]'), value: value) }
        end
        if elements && elements.length <= 100 && elements.all? { |element| element.kind == :literal && (element.value.nil? || constant(element)) }
          elements = elements.reject { |element| element.value.nil? }
          return !elements.empty? && elements.all? do |element|
            atom([left.args[0], '=', element, left], right)
          end
        end
      end
      return true if logical?(right, 'or') && (clause(left, right.args[0]) || clause(left, right.args[1]))
      comparison = comparison(left)
      comparison ? atom(comparison, right) : false
    end

    def atom(premise, predicate)
      return false unless step
      return atom(premise, predicate.args[0]) && atom(premise, predicate.args[1]) if logical?(predicate, 'and')
      return atom(premise, predicate.args[0]) || atom(premise, predicate.args[1]) if logical?(predicate, 'or')
      expression, operator, value, original = premise
      return equal?(expression, predicate.args[0]) if predicate.kind == :unary && predicate.value == 'is not null'
      target = comparison(predicate)
      return false unless target && equal?(expression, target[0]) && constant(value) == constant(target[2])
      if [operator, target[1]].any? { |op| %w[< <= > >=].include?(op) }
        return false if [original, target[3]].any? do |item|
          label = item.instance_variable_get(:@native_collation)
          label && (label.conflict || label.none || label.name != 'C')
        end
      end
      order = Types.compare(value.value, target[2].value)
      case operator
      when '=' then { '=' => order.zero?, '<>' => !order.zero?, '<' => order < 0, '<=' => order <= 0, '>' => order > 0, '>=' => order >= 0 }[target[1]]
      when '<' then %w[< <= <>].include?(target[1]) && order <= 0
      when '<=' then %w[< <>].include?(target[1]) && order < 0 || target[1] == '<=' && order <= 0
      when '>' then %w[> >= <>].include?(target[1]) && order >= 0
      when '>=' then %w[> <>].include?(target[1]) && order > 0 || target[1] == '>=' && order >= 0
      when '<>' then target[1] == '<>' && order.zero?
      else false
      end
    end

    def comparison(expression)
      return nil unless expression.kind == :binary && COMPARISONS.include?(expression.value)
      left, right = expression.args
      return [left, expression.value, right, expression] if constant(right)
      return [right, FLIPPED[expression.value], left, expression] if constant(left)
      nil
    end

    def constant(expression)
      return nil unless expression.kind == :literal && !expression.value.nil?
      return 'integer' if Types::INTEGER_BITS.key?(expression.type)
      expression.type if %w[text decimal date timestamp timestamptz boolean uuid].include?(expression.type)
    end

    def expand(expression)
      if expression.kind == :between && !expression.extra&.dig(:negated)
        value, low, high = expression.args
        return Expression.new(kind: :binary, type: 'boolean', value: 'and', args: [
          Expression.new(kind: :binary, type: 'boolean', value: '>=', args: [value, low]),
          Expression.new(kind: :binary, type: 'boolean', value: '<=', args: [value, high])])
      end
      expression
    end
  end

  module PartialPredicateProof
    def partial_predicate_implied?(partial, terms)
      !partial || PredicateImplication.new(self).conjunction(terms, partial)
    end
  end
  Engine.prepend(PartialPredicateProof)
end
