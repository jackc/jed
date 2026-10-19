# frozen_string_literal: true

require_relative 'sql_render'

module JedNative
  # Read-only relations derived from the selected database's catalog. They own no
  # pages and do not expose host state or execute stored default expressions.
  module Introspection
    CATALOG_COLUMNS = {
      'jed_tables' => [['name', 'text']],
      'jed_columns' => [['table_name', 'text'], ['name', 'text'], ['ordinal', 'i32'], ['type', 'text'], ['not_null', 'boolean'], ['pk_ordinal', 'i32'], ['column_default', 'text'], ['identity_generation', 'text']],
      'jed_indexes' => [['name', 'text'], ['table_name', 'text'], ['columns', 'text[]'], ['is_unique', 'boolean'], ['method', 'text'], ['predicate', 'text'], ['descending', 'boolean[]'], ['nulls_first', 'boolean[]']],
      'jed_constraints' => [['name', 'text'], ['table_name', 'text'], ['type', 'text'], ['columns', 'text[]'], ['expression', 'text'], ['ref_table', 'text'], ['ref_columns', 'text[]'], ['operators', 'text[]'], ['match_type', 'text']],
      'jed_statistics' => [['table_name', 'text'], ['column_name', 'text'], ['analyzed_rows', 'i64'], ['is_stale', 'boolean'], ['null_count', 'i64'], ['distinct_count', 'i64'], ['sample_rows', 'i64'], ['average_width', 'i64'], ['mcv_count', 'i32'], ['histogram_count', 'i32'], ['term_count', 'i32']],
      'jed_sequences' => [['name', 'text'], ['start_value', 'i64'], ['min_value', 'i64'], ['max_value', 'i64'], ['increment_by', 'i64'], ['cycle', 'boolean'], ['cache_size', 'i64'], ['last_value', 'i64'], ['owned_by_table', 'text'], ['owned_by_column', 'text']],
      'jed_types' => [['name', 'text'], ['kind', 'text']],
      'jed_type_fields' => [['type_name', 'text'], ['name', 'text'], ['ordinal', 'i32'], ['type', 'text'], ['not_null', 'boolean']],
      'jed_views' => [['name', 'text'], ['definition', 'text'], ['columns', 'text[]'], ['check_option', 'text'], ['security_barrier', 'boolean'], ['security_invoker', 'boolean']]
    }.transform_values { |columns| columns.map(&:freeze).freeze }.freeze

    def introspection_relation?(name) = CATALOG_COLUMNS.key?(name)

    def introspection_columns(name, database = nil, relation_alias = nil)
      return nil unless introspection_relation?(name)
      introspection_scope(database)
      CATALOG_COLUMNS.fetch(name).map { |column, type| { name: column, type: type, table: relation_alias || name } }
    end

    def source_relation(source)
      name = source[:name]
      return super unless source[:kind] == :table && introspection_relation?(name)
      return super if !source[:database] && @ctes.key?(name)
      columns = introspection_columns(name, source[:database], source[:alias])
      aliases = source[:columns] || source[:column_aliases]
      raise Error.new('42P10', 'too many column aliases') if aliases && aliases.length > columns.length
      columns = columns.each_with_index.map { |column, index| aliases&.at(index) ? column.merge(name: aliases[index]) : column }
      rows = introspection_rows(name, introspection_scope(source[:database]))
      rows.each { charge }
      Relation.new(columns: columns, rows: rows)
    end

    def dispatch(statement)
      kind = statement[:kind]
      targets = case kind
                when :insert, :update, :delete, :create_index then [statement[:table]]
                when :drop_table, :drop_view, :truncate then statement[:names] || [statement[:name] || statement[:table]]
                when :alter_table, :alter_view then [statement[:name]]
                else []
                end
      if (name = targets.find { |target| introspection_relation?(target) })
        action = %i[drop_table drop_view].include?(kind) ? 'drop' : 'modify'
        raise Error.new('42809', "cannot #{action} system relation \"#{name}\"")
      end
      if %i[create_table create_view create_index create_sequence create_type].include?(kind)
        table(statement[:table]) if kind == :create_index
        check_reserved_catalog_name(statement[:name], kind.to_s.delete_prefix('create_'))
        (statement[:constraints] || []).each do |constraint|
          check_reserved_catalog_name(constraint[:name], 'constraint') if %i[unique exclude].include?(constraint[:kind])
        end
        (statement[:columns] || []).each do |column|
          check_reserved_catalog_name(column[:unique_name], 'constraint') if column.is_a?(Hash)
        end
      elsif kind == :alter_sequence && statement[:action] == :rename
        check_reserved_catalog_name(statement[:new_name], 'sequence')
      end
      super
    end

    private

    def check_reserved_catalog_name(name, kind)
      if name&.start_with?('jed_')
        raise Error.new('42939', "#{kind} name #{name} is reserved (the jed_ prefix is reserved for system objects)")
      end
    end

    def introspection_scope(database)
      scope = database || 'main'
      raise Error.new('42P01', "database #{scope} is not attached") unless %w[main temp].include?(scope)
      scope
    end

    def catalog_tables(scope)
      @tables.values.select { |table| !!table[:temporary] == (scope == 'temp') }.sort_by { |table| table[:name].b }
    end

    def catalog_views(scope)
      @views.sort_by { |name, _| name.b }.select { |_, view| !!view[:temporary] == (scope == 'temp') }
    end

    def catalog_array(values, type = 'text') = ArrayValue.new(values, element_type: type)

    def introspection_rows(name, scope)
      tables = catalog_tables(scope)
      case name
      when 'jed_tables' then tables.map { |table| [table[:name]] }
      when 'jed_columns'
        rows = tables.flat_map do |table|
          primary = table[:constraints].find { |constraint| constraint[:kind] == :primary_key }&.dig(:columns) || []
          table[:columns].each_with_index.map do |column, index|
            key = primary.index(column[:name])
            identity = column[:identity].is_a?(Hash) ? column[:identity][:mode] : column[:identity]
            identity = identity && (identity.to_s == 'always' ? 'ALWAYS' : 'BY DEFAULT')
            [table[:name], column[:name], index + 1, introspection_type(column), column[:nullable] == false || !key.nil?,
             key && key + 1, introspection_default(column), identity]
          end
        end
        catalog_views(scope).each do |view_name, view|
          view_schema(view).each_with_index do |column, index|
            column = column.merge(default: view[:defaults]&.dig(column[:name]), view_default: true)
            rows << [view_name, column[:name], index + 1, introspection_type(column), false, nil, introspection_default(column), nil]
          end
        end
        rows
      when 'jed_indexes'
        tables.flat_map do |table|
          catalog_indexes(table).map do |index|
            elements = index[:elements] || (index[:columns] || []).map { |column| { expr: { kind: :column, name: column } } }
            method = index[:method] || 'btree'
            descending = elements.map { |element| method == 'btree' && element[:direction] == :desc }
            nulls = elements.each_with_index.map { |element, ordinal| method == 'btree' && (element[:nulls] ? element[:nulls] == :first : descending[ordinal]) }
            [index[:name], table[:name], catalog_array(elements.map { |element| catalog_expression(element[:expr]) }), !!index[:unique], method,
             index[:where] && catalog_expression(index[:where]), catalog_array(descending, 'boolean'), catalog_array(nulls, 'boolean')]
          end
        end
      when 'jed_constraints'
        tables.flat_map do |table|
          constraints = catalog_constraints(table)
          constraints.sort_by { |constraint| [%i[check unique foreign_key exclude].index(constraint[:kind]), constraint[:name].b] }.map do |constraint|
            kind = constraint[:kind]
            names = constraint[:columns] || constraint[:elements]&.map { |element| catalog_expression(element[:expr]) }
            parent = constraint[:table] || constraint[:ref_table]
            remote = constraint[:ref_columns] || constraint[:reference_columns]
            remote ||= @tables[parent]&.dig(:constraints)&.find { |item| item[:kind] == :primary_key }&.dig(:columns) if kind == :foreign_key
            expression = kind == :check ? constraint[:expr] : (kind == :exclude ? constraint[:where] : nil)
            [constraint[:name], table[:name], kind.to_s, names && catalog_array(names), expression && catalog_expression(expression),
             kind == :foreign_key ? parent : nil, kind == :foreign_key && remote ? catalog_array(remote) : nil,
             kind == :exclude ? catalog_array(constraint[:elements].map { |element| element[:op] }) : nil,
             kind == :foreign_key ? (constraint[:match] || :simple).to_s : nil]
          end
        end
      when 'jed_sequences'
        return [] unless %w[main temp].include?(scope)
        @sequences.sort_by { |sequence_name, _| sequence_name.b }.filter_map do |sequence_name, sequence|
          owner, ordinal = sequence[:owned_by]
          temporary = sequence[:temporary] || (owner && @tables[owner]&.dig(:temporary))
          next unless !!temporary == (scope == 'temp')
          [sequence_name, sequence[:start], sequence[:min_value], sequence[:max_value], sequence[:increment], !!sequence[:cycle], sequence[:cache] || 1,
           sequence[:called] ? sequence[:value] : nil, owner, owner && @tables[owner]&.dig(:columns, ordinal, :name)]
        end
      when 'jed_types'
        scope != 'main' ? [] : @types.keys.sort_by(&:b).map { |type| [type, 'composite'] }
      when 'jed_type_fields'
        scope != 'main' ? [] : @types.sort_by { |type, _| type.b }.flat_map do |type, fields|
          fields.each_with_index.map { |column, index| [type, column[:name], index + 1, introspection_type(column), column[:nullable] == false] }
        end
      when 'jed_views'
        catalog_views(scope).map do |view_name, view|
          options = view[:options] || {}
          [view_name, catalog_view_definition(view), catalog_array(view_schema(view).map { |column| column[:name] }),
           (view[:check_option] || 'NONE').to_s.upcase, !!options['security_barrier'], !!options['security_invoker']]
        end
      when 'jed_statistics'
        tables.flat_map do |table|
          (table[:storage_statistics] || []).sort_by { |stat| stat[:column] }.map do |stat|
            count = stat[:analyzed_rows] - stat[:null_count]
            [table[:name], table[:columns][stat[:column]][:name], stat[:analyzed_rows], !!stat[:stale] || table[:rows] != table[:storage_rows],
             stat[:null_count], stat[:distinct_count], stat[:sample_rows], count.zero? ? nil : (stat[:width_sum] + count - 1) / count,
             stat[:mcv].length, stat[:histogram].length, stat[:terms]&.dig(:entries)&.length]
          end
        end
      end
    end

    def catalog_constraints(table)
      used = table[:constraints].filter_map { |constraint| constraint[:name] }
      constraints = table[:constraints].filter_map do |constraint|
        next unless %i[check unique foreign_key exclude].include?(constraint[:kind])
        next constraint if constraint[:name]
        if constraint[:kind] == :unique && constraint[:columns]&.length == 1
          column = table[:columns].find { |item| item[:name] == constraint[:columns][0] }
          next constraint.merge(name: column[:unique_name]) if column && column[:unique_name]
        end
        names = constraint[:columns] || constraint[:elements]&.map { |element| element[:expr][:name] }
        names ||= catalog_column_names(constraint[:expr]).take(1) if constraint[:kind] == :check
        suffix = { check: 'check', unique: 'key', foreign_key: 'fkey', exclude: 'excl' }.fetch(constraint[:kind])
        base = ([table[:name]] + names.to_a.compact + [suffix]).join('_')
        name, counter = base, 0
        while used.include?(name)
          counter += 1
          name = "#{base}#{counter}"
        end
        used << name
        constraint.merge(name: name)
      end
      table[:indexes].each do |index|
        next unless index[:unique] && !constraints.any? { |constraint| constraint[:kind] == :unique && constraint[:name] == index[:name] }
        names = (index[:elements] || []).map { |element| catalog_expression(element[:expr]) }
        constraints << { kind: :unique, name: index[:name], columns: index[:columns] || names }
      end
      constraints
    end

    def catalog_indexes(table)
      indexes = table[:indexes].dup
      catalog_constraints(table).each do |constraint|
        next unless %i[unique exclude].include?(constraint[:kind]) && !indexes.any? { |index| index[:name] == constraint[:name] }
        indexes << { name: constraint[:name], columns: constraint[:columns], elements: constraint[:elements],
                     unique: constraint[:kind] == :unique, method: constraint[:method] || 'btree', where: constraint[:where] }
      end
      indexes.sort_by { |index| index[:name].b }
    end

    def catalog_column_names(node)
      return [] unless node.is_a?(Hash)
      return [node[:name]] if node[:kind] == :column
      node.values.flat_map { |value| value.is_a?(Array) ? value.flat_map { |item| catalog_column_names(item) } : catalog_column_names(value) }.uniq
    end

    def introspection_type(column)
      declared = column[:type]
      type = @types.key?(declared.delete_suffix('[]')) ? declared : Types.normalize(declared)
      modifier = column[:type_mod] || column[:typmod]
      return type if !modifier || type.end_with?('[]')
      modifier = { precision: modifier[0], scale: modifier[1] } if modifier.is_a?(Array)
      precision = modifier[:precision]
      case type
      when 'text' then precision ? "varchar(#{precision})" : type
      when 'decimal' then precision ? "decimal(#{precision},#{modifier[:scale] || 0})" : type
      when 'timestamp', 'timestamptz' then precision ? "#{type}(#{[precision, 6].min})" : type
      when 'interval'
        fields = modifier[:fields]
        fields = nil if fields == 0
        "interval#{fields ? " #{fields}" : ''}#{precision ? "(#{[precision, 6].min})" : ''}"
      else type
      end
    end

    def introspection_default(column)
      node = column[:default]
      return nil unless node
      return catalog_expression(node) if column[:view_default]
      if node[:kind] == :literal || node[:kind] == :unary && %w[+ -].include?(node[:op]) && node[:expr]&.dig(:kind) == :literal
        cost, scalar = @cost, @scalar_bytes
        begin
          expression = compile(node, [])
          value = assign(evaluate(expression, []), column.merge(nullable: true), expression)
          return nil if value.nil?
          text = Types.format(value, column[:type])
          return text if %w[i16 i32 i64 decimal boolean].include?(column[:type])
          return SQLRender.string(text)
        ensure
          @cost, @scalar_bytes = cost, scalar
        end
      end
      catalog_expression(node)
    end

    def catalog_view_definition(view)
      if view[:storage_view] && view[:query] == view[:storage_query]
        view[:storage_view][:definition]
      else
        catalog_query(view[:query])
      end
    end

    def catalog_expression(node, parent_precedence = 0)
      return node[:sql] if node[:sql]
      case node[:kind]
      when :column
        parts = [[node[:table], node[:table_quoted]], [node[:name], node[:quoted]]]
        parts.filter_map do |name, quoted|
          next unless name
          quoted || !name.match?(/\A[a-z_][a-z_0-9]*\z/) ? SQLRender.identifier(name) : name
        end.join(' . ')
      when :binary
        precedence = Parser::PRECEDENCE[node[:op]] || 30
        text = "#{catalog_expression(node[:left], precedence)} #{node[:op]} #{catalog_expression(node[:right], precedence + 1)}"
        text += " escape #{catalog_expression(node[:escape])}" if node[:escape]
        precedence < parent_precedence ? "( #{text} )" : text
      when :unary
        operand = catalog_expression(node[:expr] || node[:operand], 70)
        node[:op].to_s.start_with?('is ') ? "#{operand} #{node[:op]}" : "#{node[:op]} #{operand}"
      when :call
        if node[:name] == 'extract' && node[:args]&.length == 2
          return "extract ( #{catalog_expression(node[:args][0])} from #{catalog_expression(node[:args][1])} )"
        end
        arguments = (node[:args] || []).each_with_index.map do |arg, index|
          name = node[:arg_names]&.at(index)
          prefix = name ? "#{catalog_identifier(name)} => " : ''
          prefix += 'variadic ' if node[:variadic] && index == node[:args].length - 1
          prefix + catalog_expression(arg)
        end.join(' , ')
        arguments += " order by #{node[:order_by].map { |order| catalog_order(order) }.join(' , ')}" unless (node[:order_by] || []).empty?
        text = "#{catalog_identifier(node[:name])} ( #{node[:distinct] ? 'distinct ' : ''}#{arguments}#{arguments.empty? ? '' : ' '})"
        text += " within group ( order by #{node[:within_group].map { |order| catalog_order(order) }.join(' , ')} )" if node[:within_group]
        text += " filter ( where #{catalog_expression(node[:filter])} )" if node[:filter]
        text += " over #{catalog_window(node[:over])}" if node[:over]
        text
      when :star then node[:table] ? "#{catalog_identifier(node[:table], node[:table_quoted])} . *" : '*'
      when :cast
        if node[:expr][:kind] == :literal && node[:expr][:value].is_a?(String) && node[:expr][:untyped] == false
          "#{SQLRender.type_name(node[:type], node[:type_mod])} #{SQLRender.string(node[:expr][:value])}"
        else
          "#{catalog_expression(node[:expr], 70)} :: #{SQLRender.type_name(node[:type], node[:type_mod])}"
        end
      when :subquery then "( #{catalog_query(node[:query])} )"
      when :array_query then "array ( #{catalog_query(node[:query])} )"
      when :quantified
        right = node[:right][:kind] == :subquery ? catalog_query(node[:right][:query]) : catalog_expression(node[:right])
        "#{catalog_expression(node[:left], 31)} #{node[:op]} #{node[:quantifier]} ( #{right} )"
      when :subscript
        text = "( #{catalog_expression(node[:expr])} )"
        node[:specs].each do |spec|
          index = spec[:slice] ? "#{spec[:low] && catalog_expression(spec[:low])} : #{spec[:high] && catalog_expression(spec[:high])}" : catalog_expression(spec[:index])
          text += " [ #{index} ]"
        end
        text
      when :field then "( #{catalog_expression(node[:expr])} ) . #{catalog_identifier(node[:name])}"
      when :field_star then "( #{catalog_expression(node[:expr])} ) . *"
      when :collate then "#{catalog_expression(node[:expr], 70)} collate #{catalog_identifier(node[:name])}"
      when :is_json
        text = "#{catalog_expression(node[:expr], 31)} is #{node[:negated] ? 'not ' : ''}json #{node[:json_kind]}"
        text += " #{node[:unique] ? 'with' : 'without'} unique keys" unless node[:unique].nil?
        text
      when :case
        text = +'case'
        text += " #{catalog_expression(node[:operand])}" if node[:operand]
        node[:branches].each do |branch|
          condition, value = branch.is_a?(Array) ? branch : [branch[:when], branch[:then]]
          text += " when #{catalog_expression(condition)} then #{catalog_expression(value)}"
        end
        text += " else #{catalog_expression(node[:else])}" if node[:else]
        text + ' end'
      when :rollup, :cube, :grouping_sets
        name = node[:kind].to_s.tr('_', ' ')
        sets = node[:sets].map { |set| "( #{set.map { |item| catalog_expression(item) }.join(' , ')} )" }
        "#{name} ( #{sets.join(' , ')} )"
      when :exists then "exists ( #{catalog_query(node[:query])} )"
      when :in
        values = node[:query] ? catalog_query(node[:query]) : (node[:values] || node[:list]).map { |arg| catalog_expression(arg) }.join(' , ')
        "#{catalog_expression(node[:expr] || node[:left])} #{node[:negated] ? 'not ' : ''}in ( #{values} )"
      when :between
        "#{catalog_expression(node[:expr])} #{node[:negated] ? 'not ' : ''}between #{node[:symmetric] ? 'symmetric ' : ''}#{catalog_expression(node[:low] || node[:lower])} and #{catalog_expression(node[:high] || node[:upper])}"
      when :row, :array
        args = node[:args] || node[:elements] || []
        open, close = node[:kind] == :row ? ['(', ')'] : ['[', ']']
        "#{node[:kind]} #{open} #{args.map { |arg| catalog_expression(arg) }.join(' , ')}#{args.empty? ? '' : ' '}#{close}"
      else SQLRender.expression(node)
      end
    end

    def catalog_identifier(name, quoted = false)
      quoted || !name.match?(/\A[a-z_][a-z_0-9]*\z/) ? SQLRender.identifier(name) : name
    end

    def catalog_relation(source)
      text = case source[:kind]
             when :table
               [source[:database] && catalog_identifier(source[:database], source[:database_quoted]),
                catalog_identifier(source[:name], source[:quoted])].compact.join(' . ')
             when :subquery, :derived then "( #{catalog_query(source[:query])} )"
             when :function, :call then catalog_expression(source[:expr] || source)
             when :join_group
               "( #{catalog_from(source)} )"
             else raise Error.new('0A000', "catalog rendering for relation #{source[:kind]} is not implemented")
             end
      text = "lateral #{text}" if source[:lateral]
      if source[:alias]
        text += "#{source[:alias_explicit] == false ? ' ' : ' as '}#{catalog_identifier(source[:alias], source[:alias_quoted])}"
      end
      text += " ( #{source[:columns].each_with_index.map { |name, index| catalog_identifier(name, source[:columns_quoted].is_a?(Array) ? source[:columns_quoted][index] : source[:columns_quoted]) }.join(' , ')} )" if source[:columns]
      if source[:definitions]
        text += ' as' unless source[:alias]
        text += " ( #{source[:definitions].map { |column| "#{catalog_identifier(column[:name])} #{SQLRender.type_name(column[:type], column[:type_mod])}" }.join(' , ')} )"
      end
      text
    end

    def catalog_from(query)
      text = (query[:from] || []).map { |source| catalog_relation(source) }.join(' , ')
      (query[:joins] || []).each do |join|
        kind = join[:kind] == :inner ? '' : "#{join[:kind]} "
        text += " #{join[:natural] ? 'natural ' : ''}#{kind}join #{catalog_relation(join[:relation])}"
        text += " on #{catalog_expression(join[:on])}" if join[:on]
        text += " using ( #{join[:using].map { |name| catalog_identifier(name) }.join(' , ')} )" if join[:using]
      end
      text
    end

    def catalog_query(query)
      text = case query[:kind]
             when :select
               projections = query[:columns].map do |item|
                 text = catalog_expression(item[:expr])
                 generated = item[:generated_star] || item[:expr][:quoted] && item[:expr][:table_quoted] && item[:alias] == item[:expr][:name]
                 text += " as #{catalog_identifier(item[:alias], item[:alias_quoted])}" if item[:alias] && !generated
                 text
               end
               distinct = query[:distinct] ? 'distinct ' : ''
               distinct += "on ( #{query[:distinct_on].map { |node| catalog_expression(node) }.join(' , ')} ) " if query[:distinct_on]
               result = "select #{distinct}#{projections.join(' , ')}"
               result += " from #{catalog_from(query)}" unless (query[:from] || []).empty?
               result += " where #{catalog_expression(query[:where])}" if query[:where]
               result += " group by #{query[:group_by].map { |node| catalog_expression(node) }.join(' , ')}" unless (query[:group_by] || []).empty?
               result += " having #{catalog_expression(query[:having])}" if query[:having]
               unless (query[:windows] || {}).empty?
                 result += " window #{query[:windows].map { |name, window| "#{catalog_identifier(name)} as #{catalog_window(window)}" }.join(' , ')}"
               end
               result
             when :values
               'values ' + query[:rows].map { |row| "( #{row.map { |node| catalog_expression(node) }.join(' , ')} )" }.join(' , ')
             when :set_operation, :setop
               "#{catalog_set_operand(query[:left], query[:op], false)} #{query[:op]} #{query[:all] ? 'all ' : ''}#{catalog_set_operand(query[:right], query[:op], true)}"
             else raise Error.new('0A000', "catalog rendering for query #{query[:kind]} is not implemented")
             end
      unless (query[:order_by] || []).empty?
        text += " order by #{query[:order_by].map { |order| catalog_order(order) }.join(' , ')}"
      end
      text += " limit #{catalog_expression(query[:limit])}" if query[:limit]
      text += " offset #{catalog_expression(query[:offset])}" if query[:offset]
      unless (query[:with] || []).empty?
        items = query[:with].map do |cte|
          names = cte[:columns] ? " ( #{cte[:columns].each_with_index.map { |name, index| catalog_identifier(name, cte[:columns_quoted].is_a?(Array) ? cte[:columns_quoted][index] : cte[:columns_quoted]) }.join(' , ')} )" : ''
          materialized = cte[:materialized].nil? ? '' : (cte[:materialized] ? 'materialized ' : 'not materialized ')
          item = "#{catalog_identifier(cte[:name], cte[:quoted])}#{names} as #{materialized}( #{catalog_query(cte[:query])} )"
          if (search = cte[:search])
            item += " search #{search[:order]} first by #{search[:columns].map { |name| catalog_identifier(name) }.join(' , ')} set #{catalog_identifier(search[:column])}"
          end
          if (cycle = cte[:cycle])
            item += " cycle #{cycle[:columns].map { |name| catalog_identifier(name) }.join(' , ')} set #{catalog_identifier(cycle[:mark])} to #{catalog_expression(cycle[:value])} default #{catalog_expression(cycle[:default])} using #{catalog_identifier(cycle[:path])}"
          end
          item
        end
        text = "with #{query[:recursive] ? 'recursive ' : ''}#{items.join(' , ')} #{text}"
      end
      text
    end

    def catalog_set_operand(query, parent_operator, right)
      text = catalog_query(query)
      precedence = query[:op] == 'intersect' ? 2 : 1
      parent_precedence = parent_operator == 'intersect' ? 2 : 1
      nested = query[:kind] == :set_operation && (precedence < parent_precedence || right && precedence == parent_precedence)
      nested ||= query[:with] || query[:limit] || query[:offset] || !(query[:order_by] || []).empty?
      nested ? "( #{text} )" : text
    end

    def catalog_order(order)
      text = catalog_expression(order[:expr])
      text += " #{order[:direction]}" if order[:direction]
      text += " nulls #{order[:nulls]}" if order[:nulls]
      text
    end

    def catalog_window(window)
      return catalog_identifier(window) if window.is_a?(String)
      parts = []
      parts << catalog_identifier(window[:name] || window[:base]) if window[:name] || window[:base]
      parts << "partition by #{window[:partition_by].map { |node| catalog_expression(node) }.join(' , ')}" unless (window[:partition_by] || []).empty?
      parts << "order by #{window[:order_by].map { |order| catalog_order(order) }.join(' , ')}" unless (window[:order_by] || []).empty?
      if window[:frame]
        frame = window[:frame]
        bound = lambda do |node|
          case node[:kind]
          when :current_row then 'current row'
          when :unbounded then "unbounded #{node[:direction]}"
          else "#{catalog_expression(node[:value])} #{node[:direction]}"
          end
        end
        parts << "#{frame[:mode] || frame[:kind]} between #{bound.call(frame[:start])} and #{bound.call(frame[:end])}"
        parts << "exclude #{frame[:exclude].to_s.tr('_', ' ')}" if frame[:exclude]
      end
      "( #{parts.join(' ')}#{parts.empty? ? '' : ' '})"
    end
  end

  Engine.prepend(Introspection)
end
