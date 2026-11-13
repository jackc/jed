# frozen_string_literal: true

module JedNative
  module WholeRowColumns
    module_function

    def positions(scope, name)
      return [] if %w[old new excluded].include?(name)
      scope.each_index.select { |i| scope[i][:table] == name && !scope[i][:star_hidden] }
           .sort_by { |i| scope[i][:source_ordinal] || i }
    end

    def fields(columns)
      columns.map { |column| column.merge(name: column[:whole_name] || column[:name]) }
    end

    def decorate(source, columns, engine, ctes)
      return columns if source[:kind] == :join_group && !source[:alias]
      catalog = if source[:kind] == :table && !ctes.key?(source[:name])
        stored = engine.instance_variable_get(:@tables)[source[:name]]
        view = engine.instance_variable_get(:@views)[source[:name]]
        stored&.dig(:columns) || view&.dig(:schema) || view&.dig(:columns)&.map { |name| { name: name } }
      end
      columns.each_with_index.map do |column, i|
        column.merge(whole_name: catalog&.[](i)&.dig(:name) || column[:name],
                     whole_nullable: false, source_ordinal: i)
      end
    end
  end

  module WholeRowBindings
    def record_fields(node, scope)
      if node&.dig(:kind) == :column && !node[:table] && scope.none? { |column| column[:name] == node[:name] && !column[:qualified_only] }
        positions = WholeRowColumns.positions(scope, node[:name])
        unless positions.empty?
          depth = positions.map { |i| scope[i][:binding_scope] || @scope_depth }.max
          columns = positions.filter_map { |i| scope[i] if (scope[i][:binding_scope] || @scope_depth) == depth }
          return WholeRowColumns.fields(columns)
        end
      end
      super
    end

    private

    def relation(source, outer)
      WholeRowColumns.decorate(source, super, @engine, @ctes)
    end

    def expression(node, scope, wanted = nil)
      if node&.dig(:kind) == :column && !node[:table] && scope.none? { |column| column[:name] == node[:name] && !column[:qualified_only] }
        positions = WholeRowColumns.positions(scope, node[:name])
        unless positions.empty?
          depth = positions.map { |i| scope[i][:binding_scope] || @scope_depth }.max
          columns = positions.filter_map { |i| scope[i] if (scope[i][:binding_scope] || @scope_depth) == depth }
          @correlated = true if columns.any? { |column| column[:binding_external] }
          return columns.first[:type] if columns.length == 1 && columns.first[:function_label]
          raise Error.new('0A000', 'nullable whole row needs a NOT NULL column') if columns.any? { |column| column[:whole_nullable] } && columns.none? { |column| column[:whole_not_null] }
          return 'record'
        end
      end
      super
    end
  end
  ParameterBindings.prepend(WholeRowBindings)

  module WholeRows
    def source_relation(source)
      result = super
      result.columns = WholeRowColumns.decorate(source, result.columns, self, @ctes)
      result
    end

    def query_join_columns(left, right, join)
      left = left.map { |column| column.merge(whole_nullable: true) } if %i[right full right_outer full_outer].include?(join[:kind])
      right = right.map { |column| column.merge(whole_nullable: true) } if %i[left full left_outer full_outer].include?(join[:kind])
      super(left, right, join)
    end

    def join(left, source, kind, on, using)
      result = super
      left_names = left.columns.map { |column| column[:table] }.uniq
      result.columns = result.columns.map do |column|
        extended = left_names.include?(column[:table]) ? %i[right full right_outer full_outer].include?(kind) : %i[left full left_outer full_outer].include?(kind)
        extended ? column.merge(whole_nullable: true) : column
      end
      result
    end

    def compile(node, columns)
      if node[:kind] == :column && !node[:table] && !node.key?(:native_ordinal)
        scopes = [columns] + @outer.reverse.map(&:first)
        if scopes.none? { |scope| scope.any? { |column| column[:name] == node[:name] && !column[:qualified_only] } }
          scopes.each_with_index do |scope, depth|
            positions = WholeRowColumns.positions(scope, node[:name])
            next if positions.empty?
            break if positions.length == 1 && scope[positions.first][:function_label]
            selected = positions.map { |i| scope[i] }
            args = positions.map do |i|
              expression = Expression.new(kind: depth.zero? ? :column : :outer, type: scope[i][:type], value: depth.zero? ? i : [depth - 1, i])
              expression.instance_variable_set(:@record_fields, scope[i][:record_fields])
              expression
            end
            sentinel = nil
            if selected.any? { |column| column[:whole_nullable] }
              sentinel = selected.index { |column| column[:whole_not_null] }
              raise Error.new('0A000', 'nullable whole row needs a NOT NULL column') unless sentinel
            end
            fields = WholeRowColumns.fields(selected)
            result = Expression.new(kind: :row, type: 'record', args: args, extra: { whole_row: true, sentinel: sentinel, names: fields.map { |field| field[:name] } })
            result.instance_variable_set(:@record_fields, fields)
            return result
          end
        end
      end
      super
    end

    def evaluate(expression, row, group = nil)
      if expression.kind == :row && expression.extra&.dig(:whole_row)
        sentinel = expression.extra[:sentinel]
        unless sentinel.nil?
          charge(2)
          return nil if evaluate(expression.args[sentinel], row, group).nil?
        end
        charge
        return CompositeValue.new(expression.args.map { |arg| evaluate(arg, row, group) }, field_names: expression.extra[:names])
      end
      super
    end

    def validate_index_dependencies(expression)
      raise Error.new('0A000', 'whole-row references are not supported in indexes') if expression.kind == :row && expression.extra&.dig(:whole_row)
      super
    end
  end
  Engine.prepend(WholeRows)
end
