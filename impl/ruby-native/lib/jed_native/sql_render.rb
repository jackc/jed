# frozen_string_literal: true

module JedNative
  # Persist SQL as SQL tokens. No Ruby source or host object serialization is
  # admitted to a catalog entry, so another core can read the same definitions.
  module SQLRender
    module_function

    def identifier(name)
      '"' + name.to_s.gsub('"', '""') + '"'
    end

    def string(value) = "'#{value.to_s.gsub("'", "''")}'"

    def type_name(type, modifier = nil)
      base = type.to_s
      array = base.end_with?('[]')
      base = base.delete_suffix('[]') if array
      base = identifier(base) unless base.match?(/\A[a-z][a-z_0-9]*\z/)
      if modifier
        values = modifier.is_a?(Hash) ? [modifier[:precision], modifier[:scale]].compact : Array(modifier)
        base += " ( #{values.join(' , ')} )" unless values.empty?
      end
      base + (array ? ' [ ]' : '')
    end

    def expression(node)
      raise Error.new('0A000', 'missing persisted SQL expression') unless node

      case node[:kind]
      when :literal
        value = node[:value]
        return 'null' if value.nil?
        return value ? 'true' : 'false' if value == true || value == false
        return value.to_s if %w[i16 i32 i64 decimal f32 f64].include?(node[:type]) || value.is_a?(Numeric)

        string(value)
      when :column
        [node[:table], node[:name]].compact.map { |part| identifier(part) }.join(' . ')
      when :star then node[:table] ? "#{identifier(node[:table])} . *" : '*'
      when :param then "$#{node[:index]}"
      when :default then 'default'
      when :cast then "( #{expression(node[:expr])} ) :: #{type_name(node[:type], node[:type_mod])}"
      when :binary
        "( #{expression(node[:left])} #{node[:op]} #{expression(node[:right])} )"
      when :unary
        op = node[:op].to_s
        value = expression(node[:expr] || node[:operand])
        op.start_with?('is ') ? "( #{value} #{op} )" : "( #{op} #{value} )"
      when :call
        text = "#{identifier(node[:name])} ( #{node[:distinct] ? 'distinct ' : ''}#{(node[:args] || []).map { |arg| expression(arg) }.join(' , ')} )"
        text += " filter ( where #{expression(node[:filter])} )" if node[:filter]
        raise Error.new('0A000', 'persisting window expressions is not implemented') if node[:over]

        text
      when :array then "array [ #{(node[:args] || node[:elements]).map { |arg| expression(arg) }.join(' , ')} ]"
      when :row then "row ( #{(node[:args] || node[:elements]).map { |arg| expression(arg) }.join(' , ')} )"
      when :case
        text = +'case'
        text << " #{expression(node[:operand])}" if node[:operand]
        node[:branches].each do |branch|
          condition, value = branch.is_a?(Array) ? branch : [branch[:when], branch[:then]]
          text << " when #{expression(condition)} then #{expression(value)}"
        end
        text << " else #{expression(node[:else])}" if node[:else]
        text << ' end'
      when :in
        values = node[:query] ? query(node[:query]) : (node[:values] || node[:list]).map { |arg| expression(arg) }.join(' , ')
        "( #{expression(node[:expr] || node[:left])} #{node[:negated] ? 'not ' : ''}in ( #{values} ) )"
      when :between
        "( #{expression(node[:expr])} #{node[:negated] ? 'not ' : ''}between #{expression(node[:low] || node[:lower])} and #{expression(node[:high] || node[:upper])} )"
      when :subquery then "( #{query(node[:query])} )"
      when :exists then "exists ( #{query(node[:query])} )"
      when :collate then "( #{expression(node[:expr])} collate #{identifier(node[:name])} )"
      when :field then "( #{expression(node[:expr])} ) . #{identifier(node[:name])}"
      when :json_table
        text = "json_table ( #{expression(node[:context])}#{json_format(node[:format_json])} , #{expression(node[:path])}"
        text += " as #{identifier(node[:path_name])}" if node[:path_name]
        unless (node[:passing] || []).empty?
          text += ' passing ' + node[:passing].map { |binding| "#{expression(binding[:expr])}#{json_format(binding[:format_json])} as #{identifier(binding[:name])}" }.join(' , ')
        end
        text + " columns ( #{node[:columns].map { |column| json_table_column(column) }.join(' , ')} ) )"
      else raise Error.new('0A000', "persisting expression #{node[:kind]} is not implemented")
      end
    end

    def json_format(format)
      return '' unless format
      ' format json' + (format.is_a?(String) ? " encoding #{format}" : '')
    end

    def json_table_column(column)
      if column[:kind] == :nested
        name = column[:name] ? " as #{identifier(column[:name])}" : ''
        return "nested path #{string(column[:path])}#{name} columns ( #{column[:columns].map { |child| json_table_column(child) }.join(' , ')} )"
      end
      text = identifier(column[:name])
      return text + ' for ordinality' if column[:kind] == :ordinality
      text += ' ' + type_name(column[:type], column[:type_mod])
      text += ' exists' if column[:kind] == :exists
      text += json_format(column[:format_json])
      text += " path #{string(column[:path])}" if column[:path]
      text += " with #{column[:wrapper]} array wrapper" if column[:wrapper] && column[:wrapper] != :without
      text += ' omit quotes' if column[:keep_quotes] == false
      %i[on_empty on_error].each do |key|
        behavior = column[key]
        next unless behavior
        text += ' ' + behavior[:kind].to_s.tr('_', ' ')
        text += ' ' + expression(behavior[:default]) if behavior[:kind] == :default
        text += ' ' + key.to_s.tr('_', ' ')
      end
      text
    end

    def relation(source)
      text = case source[:kind]
             when :table, nil then [source[:database], source[:name]].compact.map { |name| identifier(name) }.join(' . ')
             when :subquery, :derived then "( #{query(source[:query])} )"
             when :function, :call then expression(source[:expr] || source)
             else raise Error.new('0A000', "persisting relation #{source[:kind]} is not implemented")
             end
      text += " as #{identifier(source[:alias])}" if source[:alias]
      text += " ( #{source[:columns].map { |name| identifier(name) }.join(' , ')} )" if source[:columns]
      text
    end

    def query(node)
      text = case node[:kind]
             when :select
               projections = (node[:columns] || node[:projections]).map do |item|
                 expression(item[:expr]) + (item[:alias] ? " as #{identifier(item[:alias])}" : '')
               end
               distinct = node[:distinct] ? 'distinct ' : ''
               distinct += "on ( #{node[:distinct_on].map { |key| expression(key) }.join(' , ')} ) " if node[:distinct_on]
               sql = "select #{distinct}#{projections.join(' , ')}"
               sources = node[:from] || []
               sources = [sources] unless sources.is_a?(Array)
               sql += " from #{sources.map { |source| relation(source) }.join(' , ')}" unless sources.empty?
               (node[:joins] || []).each do |join|
                 sql += " #{join[:natural] ? 'natural ' : ''}#{join[:kind]} join #{relation(join[:relation])}"
                 sql += " on #{expression(join[:on])}" if join[:on]
                 sql += " using ( #{join[:using].map { |name| identifier(name) }.join(' , ')} )" if join[:using]
               end
               sql += " where #{expression(node[:where])}" if node[:where]
               sql += " group by #{node[:group_by].map { |item| expression(item) }.join(' , ')}" if node[:group_by] && !node[:group_by].empty?
               sql += " having #{expression(node[:having])}" if node[:having]
               sql
             when :values
               'values ' + node[:rows].map { |row| "( #{row.map { |item| expression(item) }.join(' , ')} )" }.join(' , ')
             when :set_operation, :setop
               "( #{query(node[:left])} ) #{node[:op]} #{node[:all] ? 'all ' : ''}( #{query(node[:right])} )"
             else raise Error.new('0A000', "persisting query #{node[:kind]} is not implemented")
             end
      if node[:order_by] && !node[:order_by].empty?
        items = node[:order_by].map do |order|
          expression(order[:expr]) + " #{order[:direction] || :asc}" + (order[:nulls] ? " nulls #{order[:nulls]}" : '')
        end
        text += " order by #{items.join(' , ')}"
      end
      text += " limit #{expression(node[:limit])}" if node[:limit]
      text += " offset #{expression(node[:offset])}" if node[:offset]
      if node[:with] && !node[:with].empty?
        items = node[:with].map do |cte|
          cols = cte[:columns] ? " ( #{cte[:columns].map { |name| identifier(name) }.join(' , ')} )" : ''
          "#{identifier(cte[:name])}#{cols} as ( #{query(cte[:query])} )"
        end
        text = "with #{node[:recursive] ? 'recursive ' : ''}#{items.join(' , ')} #{text}"
      end
      text
    end
  end
end
