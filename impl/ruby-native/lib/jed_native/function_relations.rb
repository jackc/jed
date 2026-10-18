# frozen_string_literal: true

module JedNative
  module FunctionRelationColumns
    module_function

    def decorate(source, columns)
      if %i[function call srf].include?(source[:kind])
        call = source[:expr] || source[:call] || source
        name = call[:name]
        if columns.length == 1 && name && !name.include?('_record')
          label = source[:alias] || name
          return columns.map { |column| column.merge(function_label: label) }
        end
      elsif source[:kind] == :join_group && source[:alias]
        return columns.map { |column| column.reject { |key, _| key == :function_label } }
      end
      columns
    end
  end

  module FunctionRelationBindings
    private

    def relation(source, outer)
      FunctionRelationColumns.decorate(source, super)
    end

    def expression(node, scope, wanted = nil)
      if node.is_a?(Hash) && node[:kind] == :column && node[:name] && !node[:table] &&
         scope.none? { |column| column[:name] == node[:name] && !column[:qualified_only] }
        matches = scope.select { |column| column[:function_label] && column[:function_label] == node[:name] }
        unless matches.empty?
          depth = matches.map { |column| column[:binding_scope] || @scope_depth }.max
          matches.select! { |column| (column[:binding_scope] || @scope_depth) == depth }
          raise Error.new('42702', 'ambiguous function relation') if matches.length > 1
          @correlated = true if matches.first[:binding_external]
          return matches.first[:type]
        end
      end
      super
    end
  end
  ParameterBindings.prepend(FunctionRelationBindings)

  module FunctionRelations
    def source_relation(source)
      result = super
      aliases = source[:columns] || source[:column_aliases]
      raise Error.new('42P10', 'too many relation column aliases') if aliases && aliases.length > result.columns.length
      result.columns = FunctionRelationColumns.decorate(source, result.columns)
      result
    end

    def compile(node, columns)
      if node[:kind] == :column && node[:name] && !node[:table]
        scopes = [columns] + @outer.reverse.map(&:first)
        if scopes.none? { |scope| scope.any? { |column| column[:name] == node[:name] && !column[:qualified_only] } }
          scopes.each_with_index do |scope, depth|
            positions = scope.each_index.select { |index| scope[index][:function_label] && scope[index][:function_label] == node[:name] }
            next if positions.empty?
            raise Error.new('42702', 'ambiguous function relation') if positions.length > 1
            column = scope[positions.first]
            expression = Expression.new(kind: depth.zero? ? :column : :outer, type: column[:type],
                                        value: depth.zero? ? positions.first : [depth - 1, positions.first])
            expression.instance_variable_set(:@native_collation, column[:collation_label]) if column[:collation_label]
            expression.instance_variable_set(:@record_fields, column[:record_fields]) if column[:record_fields]
            return expression
          end
        end
      end
      super
    end
  end
  Engine.prepend(FunctionRelations)
end
