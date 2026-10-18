# frozen_string_literal: true

require_relative 'unicode_data'

module JedNative
  CollationLabel = Struct.new(:name, :explicit, :conflict, :table, :default, :none, keyword_init: true)

  module CollationRules
    module_function
    def combine(labels)
      labels = labels.compact
      return nil if labels.empty?
      explicit = labels.select(&:explicit)
      if !explicit.empty?
        raise Error.new('42P21', 'explicit collation mismatch') if explicit.map(&:name).uniq.length > 1
        return explicit.first
      end
      present = labels.reject(&:none)
      return labels.first if present.empty?
      labels = present
      labels = labels.reject(&:default) if labels.any? { |label| !label.default }
      return CollationLabel.new(conflict: true).freeze if labels.any?(&:conflict) || labels.map(&:name).uniq.length > 1
      labels.first
    end
    def label(value)
      return value.collation_label if value.is_a?(CollatedText)
      return combine(value.map { |item| label(item) }) if value.is_a?(Array)
      nil
    end
    def wrap(value, label)
      return value unless label
      if value.is_a?(String) && !value.is_a?(Bytea) && !value.is_a?(UUID)
        CollatedText.new(value, label)
      elsif value.is_a?(ArrayValue) && value.element_type == 'text'
        ArrayValue.new(value.map { |item| item && CollatedText.new(item, label) }, element_type: 'text', dimensions: value.dimensions, lower_bounds: value.lower_bounds)
      else value
      end
    end
  end

  # This transient string keeps expression metadata across materialized rows.
  # SQL equality and stored bytes remain exactly ordinary UTF-8 string bytes.
  class CollatedText < String
    attr_reader :collation_label
    def initialize(text, label)
      super(text)
      @collation_label = label
    end
  end

  module CollationComparison
    def compare(left, right)
      if left.is_a?(String) && right.is_a?(String)
        label = CollationRules.combine([CollationRules.label(left), CollationRules.label(right)])
        if label
          raise Error.new('42P22', 'indeterminate collation') if label.conflict || label.none
          return label.table.sort_key(left) <=> label.table.sort_key(right) if label.table
        end
      end
      super
    end
  end
  Types.singleton_class.prepend(CollationComparison)

  module UnicodeFunctions
    def call(name, args, context: nil, arg_types: nil)
      if %w[lower upper].include?(name) && (args.first.nil? || args.first.is_a?(String))
        return nil if args.first.nil?
        return context ? context.fold_case(args.first, name.to_sym) : args.first.tr(name == 'upper' ? 'a-z' : 'A-Z', name == 'upper' ? 'A-Z' : 'a-z')
      end
      super
    end
    def like(text, pattern, escape: '\\', insensitive: false, context: nil)
      if insensitive && context
        text = context.fold_case(text, :lower, simple: true)
        pattern = context.fold_case(pattern, :lower, simple: true)
        insensitive = false
      end
      super
    end
  end
  Functions.singleton_class.prepend(UnicodeFunctions)

  module Collations
    def default_collation = @default_collation || 'C'
    def default_collation=(name)
      resolve_collation(name)
      @native_select_plans&.clear if default_collation != name
      @default_collation = name.dup.freeze
    end
    def resolve_collation(name)
      return nil if name == 'C'
      @environment ||= Environment.new
      @environment.collations[name] || raise(Error.new('42704', "collation #{name} is not loaded"))
    end

    def fold_case(text, direction, simple: false)
      @environment ||= Environment.new
      properties = @environment.unicode_properties
      return properties.fold(text, direction, simple: simple) if properties
      direction == :upper ? text.tr('a-z', 'A-Z') : text.tr('A-Z', 'a-z')
    end

    def collatable_type?(type) = type == 'text' || type == 'text[]'
    def table_columns(table, name = table[:name])
      super.each_with_index.map { |column, index| column.merge(collation: table[:columns][index][:collation]) }
    end
    def collation_label(name, explicit: false, default: false)
      CollationLabel.new(name: name, explicit: explicit, conflict: false, default: default, table: resolve_collation(name)).freeze
    end

    def create_table(statement)
      if statement[:columns]
        statement = statement.merge(columns: statement[:columns].map do |column|
          collatable_type?(Types.normalize(column[:type])) && !column[:collation] ? column.merge(collation: default_collation) : column
        end)
      end
      (statement[:columns] || []).each do |column|
        next unless column[:collation]
        raise Error.new('42804', 'collation requires a text type') unless collatable_type?(Types.normalize(column[:type]))
        resolve_collation(column[:collation])
      end
      super
    end

    def compile(node, columns)
      if node[:kind] == :collate
        expression = compile(node[:expr], columns)
        raise Error.new('42804', 'collation requires a text type') unless collatable_type?(expression.type) || %w[null unknown].include?(expression.type)
        expression.type = 'text' if %w[null unknown].include?(expression.type)
        label = collation_label(node[:name], explicit: true)
        expression = Expression.new(kind: :collate, type: expression.type, args: [expression], value: label)
        expression.instance_variable_set(:@native_collation, label)
        return expression
      end
      expression = super
      if expression.kind == :binary && expression.value.include?('like') && !expression.args.all? { |arg| %w[text null unknown].include?(arg.type) }
        raise Error.new('42804', 'LIKE operands must be text')
      end
      if expression.kind == :column
        column = columns[expression.value]
        label = column[:collation_label] || (collation_label(column[:collation] || 'C') if collatable_type?(expression.type))
      elsif expression.kind == :outer
        column = @outer[-1 - expression.value[0]][0][expression.value[1]]
        label = column[:collation_label] || (collation_label(column[:collation]) if column[:collation])
      else
        children = if expression.kind == :case
          expression.args[1].map(&:last) + [expression.args[2]]
        else
          expression.args.is_a?(Array) ? expression.args.flatten.select { |item| item.is_a?(Expression) } : []
        end
        children = expression.extra[:aggregate_order].map(&:first) if expression.kind == :call && expression.extra&.dig(:within_group)
        children = children.compact
        label = CollationRules.combine(children.map { |child| child.instance_variable_get(:@native_collation) })
        label ||= CollationRules.label(expression.value) if expression.kind == :literal
        label ||= collation_label(default_collation, default: true) if expression.kind == :literal && collatable_type?(expression.type)
      end
      label = collation_label(default_collation, default: true) if expression.kind == :cast && expression.extra&.dig(:type_mod) && label&.none
      expression.instance_variable_set(:@native_collation, label) if label
      if expression.kind == :binary && expression.value != '||' && label&.conflict
        raise Error.new('42P22', 'indeterminate collation')
      end
      if expression.kind == :call && %w[greatest least nullif min max width_bucket].include?(expression.value) && (label&.conflict || label&.none)
        raise Error.new('42P22', 'indeterminate collation')
      end
      expression
    end

    def cast_value(value, type, source_type: nil)
      return value if type == 'text' && value.is_a?(CollatedText)
      super
    end

    def evaluate(expression, row, group = nil)
      if expression.kind == :collate
        return CollationRules.wrap(evaluate(expression.args[0], row, group), expression.value)
      elsif expression.kind == :binary && %w[< <= > >= = <> != like ilike not\ like not\ ilike].include?(expression.value) && expression.args.any? { |arg| collatable_type?(arg.type) }
        charge
        left, right = expression.args.map { |arg| evaluate(arg, row, group) }
        return nil if left.nil? || right.nil?
        if expression.value.include?('like')
          answer = Functions.like(left, right, insensitive: expression.value.include?('ilike'), context: self)
          return expression.value.start_with?('not ') ? !answer : answer
        end
        label = expression.instance_variable_get(:@native_collation) || CollationRules.combine([CollationRules.label(left), CollationRules.label(right)])
        if %w[= <> !=].include?(expression.value)
          scalar_work(expression.value, left, right) if respond_to?(:scalar_work)
          answer = left == right
          return expression.value == '=' ? answer : !answer
        end
        raise Error.new('42P22', 'indeterminate collation') if label&.conflict || label&.none
        if label&.table
          charge(collation_comparison_points(left, right))
          left, right = CollationRules.wrap(left, label), CollationRules.wrap(right, label)
        elsif respond_to?(:scalar_work)
          scalar_work(expression.value, left, right)
        end
        return Types.binary(expression.value, left, right, type: expression.type)
      end
      result = super
      return result unless collatable_type?(expression.type)
      CollationRules.wrap(result, expression.instance_variable_get(:@native_collation))
    end

    def collation_points(value)
      return value.length if value.is_a?(String)
      return value.sum { |item| collation_points(item) } if value.is_a?(Array)
      0
    end

    def collation_comparison_points(left, right)
      return collation_points(left) + collation_points(right) unless left.is_a?(Array) && right.is_a?(Array)
      cost = 0
      left.zip(right).take([left.length, right.length].min).each do |a, b|
        break if a.nil? || b.nil?
        cost += collation_comparison_points(a, b)
        break unless Types.compare(a, b).zero?
      end
      cost
    end

    def order_compare(left, right, order)
      if !left.nil? && !right.nil? && (label = CollationRules.combine([CollationRules.label(left), CollationRules.label(right)]))
        raise Error.new(label.conflict ? '42P21' : '42P22', 'indeterminate collation') if label.conflict || label.none
        charge(collation_comparison_points(left, right)) if label.table
      end
      super
    end
  end
  Engine.prepend(Collations)
end
