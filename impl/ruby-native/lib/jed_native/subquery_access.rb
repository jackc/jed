# frozen_string_literal: true

module JedNative
  module SubqueryAccess
    def scan_access_plan(stored, predicate, statement = nil)
      previous = @native_estimate_membership
      @native_estimate_membership = statement && statement[:kind] == :select
      super
    ensure
      @native_estimate_membership = previous
    end

    def source_relation(source)
      previous = @native_derived_membership
      @native_derived_membership = true if %i[subquery derived].include?(source[:kind])
      super
    ensure
      @native_derived_membership = previous
    end

    private

    def reduce_intervals(expression, key, encoder)
      return [[nil, nil]] if expression.kind == :query_membership && expression.extra[:native_unfolded] && interval_set_source?(expression) && same_key_expression?(key, expression.args.first)
      super
    end

    def interval_set_source?(expression)
      return super unless expression.kind == :query_membership
      return false unless super
      data = expression.extra
      left = expression.args.first
      label = left.instance_variable_get(:@native_collation)
      return false if !data[:quantified] && label && label.name != 'C'
      type = data[:element_type]
      !type || type == left.type || Types::INTEGER_BITS.key?(left.type) && (Types::INTEGER_BITS.key?(type) || type == 'decimal') || left.type == 'decimal' && Types::INTEGER_BITS.key?(type)
    end

    def expression_work(expression)
      work = super
      if !@native_estimate_membership && expression.is_a?(Expression) && expression.kind == :query_membership && !expression.extra[:correlated] && expression.extra[:rows]
        work += expression.extra[:rows].length
      end
      work
    end

    def estimate_predicate(rows, expression)
      return (rows + 2) / 3 if @native_estimate_membership && expression&.kind == :query_membership
      return super unless expression&.kind == :query_membership && !expression.extra[:correlated] && expression.extra[:rows]
      data = expression.extra
      values = data[:rows].map(&:first)
      return rows if values.empty? && (data[:negated] || data[:quantifier] == 'all')
      return 0 if data[:negated] && values.any?(&:nil?)
      estimate = [rows, values.count { |value| !value.nil? } * ((rows + 199) / 200)].min
      data[:negated] ? rows - estimate : estimate
    end
  end
  Engine.prepend(SubqueryAccess)
end
