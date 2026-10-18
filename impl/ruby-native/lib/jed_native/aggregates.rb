# frozen_string_literal: true

module JedNative
  module AggregateCatalog
    EXTRA_AGGREGATES = %w[mode percentile_cont percentile_disc rank dense_rank percent_rank cume_dist json_object_agg jsonb_object_agg json_object_agg_strict jsonb_object_agg_strict json_object_agg_unique jsonb_object_agg_unique json_object_agg_unique_strict jsonb_object_agg_unique_strict].freeze
    def aggregate?(name)
      EXTRA_AGGREGATES.include?(name.to_s) || super
    end
  end
  Functions.singleton_class.prepend(AggregateCatalog)

  module AggregateExecution
    ORDERED = %w[mode percentile_cont percentile_disc].freeze
    HYPOTHETICAL = %w[rank dense_rank percent_rank cume_dist].freeze
    MOMENTS = %w[var_pop var_samp variance stddev_pop stddev_samp stddev].freeze
    PAIRS = %w[covar_pop covar_samp corr regr_count regr_avgx regr_avgy regr_sxx regr_syy regr_sxy regr_slope regr_intercept regr_r2].freeze
    NUMERIC = %w[i16 i32 i64 decimal f32 f64 unknown null].freeze

    def select(statement)
      old_states = @aggregate_group_states
      old_cache = @aggregate_cached_results
      @aggregate_group_states = {}
      @aggregate_cached_results = {}
      [statement[:where], *(statement[:joins] || []).map { |join| join[:on] }].each do |value|
        raise Error.new('42803', 'aggregate functions are not allowed in this clause') if local_aggregate?(value)
      end
      super
    ensure
      @aggregate_group_states = old_states
      @aggregate_cached_results = old_cache
    end

    def evaluate(expression, row, group = nil)
      state = group && @aggregate_group_states && @aggregate_group_states[group.object_id]
      if expression.kind == :grouping
        raise Error.new('42803', 'GROUPING requires a grouped query') unless state
        return expression.args.reduce(0) do |mask, argument|
          signature = aggregate_expression_key(argument)
          raise Error.new('42803', 'GROUPING argument must be a grouping expression') unless state[:keys].key?(signature)
          (mask << 1) | (state[:active].include?(signature) ? 0 : 1)
        end
      end
      if state && expression.kind != :call
        signature = aggregate_expression_key(expression)
        return state[:keys][signature] if state[:keys].key?(signature)
      end
      super
    end

    def resolve_group_nodes(statement, columns)
      aliases = (statement[:columns] || []).to_h { |item| [item[:alias], item[:expr]] }
      resolve = lambda do |node|
        if node[:kind] == :literal && node[:value].is_a?(Integer)
          index = node[:value] - 1
          raise Error.new('42P10', 'GROUP BY position out of range') unless index.between?(0, statement[:columns].length - 1)
          statement[:columns][index][:expr]
        elsif node[:kind] == :column && !node[:table] && columns.none? { |column| column[:name] == node[:name] } && aliases[node[:name]]
          aliases[node[:name]]
        elsif %i[rollup cube grouping_sets].include?(node[:kind])
          node.merge(sets: node[:sets].map { |set| set.map { |item| resolve.call(item) } })
        else
          node
        end
      end
      nodes = (statement[:group_by] || []).map { |node| resolve.call(node) }
      return nodes if nodes.empty? || nodes.any? { |node| %i[rollup cube grouping_sets].include?(node[:kind]) }

      # A complete base-table primary key determines the remaining columns.
      # They become ordinary master grouping keys, including in joins; derived
      # relations and grouping sets do not confer this dependency.
      slots = nodes.filter_map { |node| compile(node, columns).value if node[:kind] == :column }
      sources = (statement[:from] || []) + (statement[:joins] || []).map { |join| join[:relation] }
      sources.each do |source|
        next unless source[:kind] == :table && !@ctes.key?(source[:name]) && (stored = @tables[source[:name]])
        primary = stored[:constraints].find { |constraint| constraint[:kind] == :primary_key }
        next unless primary && !primary[:columns].empty?
        label = source[:alias] || source[:name]
        primary_slots = primary[:columns].map { |name| columns.index { |column| column[:table] == label && column[:name] == name } }
        next unless primary_slots.all? { |slot| slot && slots.include?(slot) }

        columns.each_with_index do |column, slot|
          next unless column[:table] == label && !slots.include?(slot)
          nodes << { kind: :column, name: column[:name], table: label }
          slots << slot
        end
      end
      nodes
    end

    def validate_aggregate_scope(statement, columns, grouped)
      [statement[:where], *(statement[:joins] || []).map { |join| join[:on] }].each do |value|
        raise Error.new('42803', 'aggregates are not allowed in this clause') if local_aggregate?(value)
      end
      projections = statement[:columns] || []
      aliases = projections.to_h { |item| [item[:alias] || expression_name(item[:expr]), item[:expr]] }
      if statement[:distinct]
        (statement[:order_by] || []).each do |order|
          key = order[:expr]
          next if key[:kind] == :literal && key[:value].is_a?(Integer)
          key = aliases[key[:name]] if key[:kind] == :column && !key[:table] && aliases[key[:name]]
          raise Error.new('42P10', 'DISTINCT ORDER BY expressions must appear in the select list') unless projections.any? { |item| item[:expr] == key || item[:expr][:kind] == :star }
        end
      end
      return unless grouped
      nodes = resolve_group_nodes(statement, columns)
      keys = expand_grouping_sets(nodes).flatten(1).uniq
      keys.each { |key| raise Error.new('42803', 'GROUP BY cannot contain aggregate functions') if local_aggregate?(key) }
      allowed = keys.filter_map do |key|
        compile(key, columns).value if key[:kind] == :column
      end
      sources = (statement[:from] || []) + (statement[:joins] || []).map { |join| join[:relation] }
      sources.each do |source|
        next unless source[:kind] == :table && @tables.key?(source[:name])
        key = @tables[source[:name]][:constraints].find { |constraint| constraint[:kind] == :primary_key }
        next unless key
        label = source[:alias] || source[:name]
        primary_slots = key[:columns].map { |name| columns.index { |column| column[:table] == label && column[:name] == name } }
        allowed.concat(columns.each_index.select { |index| columns[index][:table] == label }) if primary_slots.all? { |slot| allowed.include?(slot) }
      end
      validator = lambda do |value|
        if value.is_a?(Array)
          value.each { |child| validator.call(child) }
        elsif value.is_a?(Hash)
          next if %i[subquery exists array_query].include?(value[:kind])
          next if keys.include?(value)
          if value[:kind] == :call && value[:name] == 'grouping'
            raise Error.new('42803', 'GROUPING arguments must be grouping expressions') unless value[:args].all? { |arg| keys.include?(arg) }
            next
          end
          if value[:kind] == :call && Functions.aggregate?(value[:name]) && !value[:over]
            value[:args].each { |arg| validator.call(arg) } if value[:within_group]
            next
          end
          if value[:kind] == :column
            slot = compile(value, columns).value
            raise Error.new('42803', "column #{value[:name]} must appear in GROUP BY") unless allowed.include?(slot)
          end
          value.each { |key, child| validator.call(child) unless %i[query with].include?(key) }
        end
      end
      validator.call(statement[:columns])
      validator.call(statement[:having])
      validator.call(statement[:windows])
      (statement[:order_by] || []).each do |order|
        key = order[:expr]
        key = aliases[key[:name]] if key[:kind] == :column && !key[:table] && aliases[key[:name]]
        validator.call(key)
      end
    end

    def build_aggregate_contexts(statement, relation, group_nodes)
      sets = expand_grouping_sets(group_nodes)
      keys = sets.flatten(1).uniq
      compiled = keys.to_h { |key| [key, compile(key, relation.columns)] }
      key_values = relation.rows.map { |row| keys.to_h { |key| [key, evaluate(compiled[key], row)] } }
      having = compile_filter(statement[:having], relation.columns)
      materialized = projection_aggregates([statement[:columns], statement[:having], statement[:order_by]]).map { |node| compile(node, relation.columns) }
      contexts = []
      if @query_memory
        rows_owner = query_memory_owner
        group_owners = []
        vector = @query_memory_contexts.last[:vector_group]
        synthetic_rows = []
      end
      sets.each do |set|
        groups = {}
        owners = {} if @query_memory
        relation.rows.each_with_index do |row, index|
          key = set.map { |item| key_values[index][item] }
          if @query_memory && !groups.key?(key)
            owners[key] = query_memory_owner
            query_memory_add(owners[key], MemorySchedule::ENTRY + query_memory_key_bytes(key))
            group_owners << owners[key]
          end
          (groups[key] ||= []) << row
        end
        if @query_memory && set.empty? && groups.empty?
          owners[[]] = query_memory_owner
          query_memory_add(owners[[]], MemorySchedule::ENTRY)
          group_owners << owners[[]]
        end
        groups[[]] ||= [] if set.empty?
        groups.each do |values, group|
          row = (group.first || Array.new(relation.columns.length)).dup
          state = { keys: {}, active: [] }
          keys.each do |key|
            expression = compiled[key]
            signature = aggregate_expression_key(expression)
            index = set.index(key)
            state[:keys][signature] = index ? values[index] : nil
            state[:active] << signature if index
            row[expression.value] = nil if !index && expression.kind == :column
          end
          @aggregate_group_states[group.object_id] = state
          if @query_memory
            previous_aggregate_owner = @query_memory_aggregate_owner
            @query_memory_aggregate_owner = owners[values]
          end
          aggregate_values = materialized.map { |expression| aggregate(expression, group) }
          accepted = !having || evaluate(having, row, group) == true
          if @query_memory
            @query_memory_aggregate_owner = previous_aggregate_owner
            query_memory_release(owners[values]) unless vector
            synthetic = keys.map { |key| state[:keys][aggregate_expression_key(compiled[key])] } + aggregate_values
            if accepted
              vector ? synthetic_rows << synthetic : query_memory_add_row(rows_owner, synthetic)
            end
          end
          next unless accepted
          contexts << [row, group]
        end
      end
      if @query_memory
        group_owners.each { |owner| query_memory_release(owner) } if vector
        synthetic_rows.each { |row| query_memory_add_row(rows_owner, row) } if vector
        query_memory_release(relation)
      end
      contexts
    end

    def expand_grouping_sets(nodes)
      result = [[]]
      nodes.each do |node|
        choices = case node[:kind]
                  when :grouping_sets then node[:sets]
                  when :rollup then node[:sets].length.downto(0).map { |count| node[:sets].first(count).flatten }
                  when :cube
                    raise Error.new('54001', 'too many grouping sets') if node[:sets].length > 12
                    ((1 << node[:sets].length) - 1).downto(0).map do |mask|
                      node[:sets].each_with_index.flat_map { |set, index| (mask & (1 << (node[:sets].length - index - 1))).zero? ? [] : set }
                    end
                  else [[node]]
                  end
        raise Error.new('54001', 'too many grouping sets') if result.length * choices.length > 4096
        result = result.flat_map { |prefix| choices.map { |suffix| (prefix + suffix).uniq } }
      end
      result
    end

    def aggregate_expression_key(expression)
      [expression.kind, expression.type, expression.value,
       (expression.args || []).map { |arg| arg.is_a?(Expression) ? aggregate_expression_key(arg) : arg }]
    end

    def local_aggregate?(value)
      return value.any? { |child| local_aggregate?(child) } if value.is_a?(Array)
      return false unless value.is_a?(Hash)
      return false if %i[subquery exists array_query].include?(value[:kind])
      return true if value[:kind] == :call && Functions.aggregate?(value[:name]) && !value[:over]
      value.values.any? { |child| local_aggregate?(child) }
    end

    def projection_aggregates(value, result = [])
      if value.is_a?(Array)
        value.each { |child| projection_aggregates(child, result) }
      elsif value.is_a?(Hash)
        return result if %i[subquery exists array_query].include?(value[:kind])
        if value[:kind] == :call && Functions.aggregate?(value[:name]) && !value[:over]
          result << value
        else
          value.each { |key, child| projection_aggregates(child, result) unless %i[with query].include?(key) }
        end
      end
      result
    end

    def compile(node, columns)
      return super unless node && node[:kind] == :call
      name = node[:name]
      if name == 'grouping'
        raise Error.new('42601', 'invalid GROUPING syntax') if node[:over] || node[:distinct] || (node[:args] || []).empty? || node[:args].any? { |arg| arg[:kind] == :star }
        arguments = node[:args].map { |arg| compile(arg, columns) }
        raise Error.new('42803', 'GROUPING cannot refer to outer grouping columns') if arguments.any? { |argument| argument.kind == :outer }
        return Expression.new(kind: :grouping, type: 'i32', args: arguments)
      end
      is_aggregate = Functions.aggregate?(name)
      raise Error.new('42809', 'DISTINCT is only valid for aggregate functions') if node[:distinct] && !is_aggregate
      if node[:order_by] && !node[:order_by].empty? && !is_aggregate
        raise Error.new('42809', 'ORDER BY is only valid for aggregate functions')
      end
      if node[:filter] && !is_aggregate && !node[:over]
        raise Error.new('42809', 'FILTER is only valid for aggregate functions')
      end
      if node[:within_group]
        raise Error.new('42883', "#{name} is not an ordered-set aggregate") unless (ORDERED + HYPOTHETICAL).include?(name)
        raise Error.new('42601', 'WITHIN GROUP cannot be combined with DISTINCT or argument ORDER BY') if node[:distinct] || (node[:order_by] && !node[:order_by].empty?)
        raise Error.new('0A000', 'ordered-set window aggregates are not supported') if node[:over]
        return compile_ordered_aggregate(node, columns)
      end
      raise Error.new('42883', "#{name} requires WITHIN GROUP") if ORDERED.include?(name)
      return super unless is_aggregate && !HYPOTHETICAL.include?(name)
      args = node[:args] || []
      raise Error.new('42803', 'aggregate FILTER cannot contain aggregate functions') if contains_aggregate?(node[:filter])
      wanted = PAIRS.include?(name) || name == 'string_agg' || name.include?('object_agg') ? 2 : 1
      raise Error.new('42883', "incorrect arguments for #{name}") unless args.length == wanted
      if args.any? { |arg| arg[:kind] == :star } && name != 'count'
        raise Error.new('42883', "#{name} does not accept *")
      end
      if !node[:over] && args.any? { |arg| contains_aggregate?(arg) }
        raise Error.new('42803', 'aggregate function calls cannot be nested')
      end
      expression = super
      types = expression.args.map { |arg| Types.base(arg.type) }
      if (%w[sum avg] + MOMENTS + PAIRS).include?(name) && types.any? { |type| !NUMERIC.include?(type) && !(type == 'interval' && %w[sum avg].include?(name)) }
        raise Error.new('42883', "#{name} requires numeric arguments")
      elsif %w[bool_and bool_or every].include?(name) && types.any? { |type| !%w[boolean unknown null].include?(type) }
        raise Error.new('42883', "#{name} requires boolean arguments")
      elsif %w[bit_and bit_or bit_xor].include?(name) && types.any? { |type| !%w[i16 i32 i64 unknown null].include?(type) }
        raise Error.new('42883', "#{name} requires integer arguments")
      elsif name == 'string_agg'
        if types.first == 'bytea' && expression.args[1].extra.is_a?(Hash) && expression.args[1].extra[:untyped]
          expression.args[1].type = 'bytea'
          expression.args[1].value = cast_value(expression.args[1].value, 'bytea')
          types[1] = 'bytea'
        end
        unless types.all? { |type| %w[text varchar char unknown null].include?(type) } || types.all? { |type| %w[bytea unknown null].include?(type) }
          raise Error.new('42883', 'string_agg arguments must both be text or bytea')
        end
        expression.type = types.include?('bytea') ? 'bytea' : 'text'
      elsif name == 'array_agg' && %w[unknown null].include?(types.first)
        raise Error.new('42725', 'array_agg(NULL) is ambiguous')
      elsif %w[min max].include?(name) && %w[json jsonpath].include?(types.first)
        raise Error.new('42883', 'type has no ordering operator')
      end
      raise Error.new('42883', 'DISTINCT input has no equality operator') if node[:distinct] && types.any? { |type| %w[json jsonpath].include?(type) }
      expression.type = %w[f32 f64].include?(types.first) ? 'f64' : 'decimal' if MOMENTS.include?(name)
      expression.type = name == 'regr_count' ? 'i64' : 'f64' if PAIRS.include?(name)
      if name.start_with?('jsonb_')
        expression.type = 'jsonb'
      elsif name.start_with?('json_')
        expression.type = 'json'
      end
      if node[:order_by]
        node[:order_by].each do |order|
          if local_aggregate?(order[:expr]) || (respond_to?(:window_nodes, true) && !window_nodes(order[:expr]).empty?)
            raise Error.new('42803', 'aggregate ORDER BY cannot contain aggregates or window functions')
          end
          if node[:distinct] && args.none? { |arg| arg == order[:expr] }
            raise Error.new('42P10', 'DISTINCT aggregate ORDER BY expressions must be arguments')
          end
        end
        expression.extra = expression.extra.merge(aggregate_order: node[:order_by].map do |order|
          compiled = compile(order[:expr], columns)
          raise Error.new('42883', 'type has no ordering operator') if %w[json jsonpath].include?(compiled.type)
          [compiled, order]
        end)
      end
      expression.extra = expression.extra.merge(aggregate_cache_id: node.object_id) unless node[:over]
      expression
    end

    def compile_ordered_aggregate(node, columns)
      name = node[:name]
      args = (node[:args] || []).map { |arg| compile(arg, columns) }
      raise Error.new('42803', 'ordered-set ordering key cannot contain aggregates') if node[:within_group].any? { |order| local_aggregate?(order[:expr]) }
      orders = node[:within_group].map { |order| [compile(order[:expr], columns), order] }
      if HYPOTHETICAL.include?(name)
        raise Error.new('42883', 'hypothetical row and ordering keys must have equal widths') unless args.length == orders.length
        args.zip(orders).each do |arg, (key, _)|
          common_type(arg.type, key.type)
        end
        type = %w[rank dense_rank].include?(name) ? 'i64' : 'f64'
      else
        raise Error.new('42883', 'ordered-set aggregate requires one ordering key') unless orders.length == 1
        raise Error.new('42883', 'incorrect ordered-set direct argument count') unless args.length == (name == 'mode' ? 0 : 1)
        input = orders.first.first.type
        raise Error.new('42883', 'type has no ordering operator') if %w[json jsonpath].include?(input)
        if name == 'percentile_cont'
          raise Error.new('42883', 'percentile_cont requires numeric or interval input') unless NUMERIC.include?(input) || input == 'interval'
          type = input == 'interval' ? 'interval' : 'f64'
        else
          type = input
        end
        type += '[]' if args.first&.type&.end_with?('[]')
      end
      Expression.new(kind: :call, type: type, value: name, args: args,
                     extra: node.merge(aggregate_cache_id: node.object_id, aggregate_order: orders, filter_expr: compile_boolean_value(node[:filter], columns)))
    end

    def aggregate(expression, group)
      if @aggregate_cached_results && expression.extra[:aggregate_cache_id]
        key = [expression.extra[:aggregate_cache_id], group.object_id]
        return @aggregate_cached_results[key] if @aggregate_cached_results.key?(key)
        return @aggregate_cached_results[key] = compute_aggregate(expression, group)
      end
      compute_aggregate(expression, group)
    end

    def compute_aggregate(expression, group)
      return ordered_aggregate(expression, group) if expression.extra[:within_group]
      rows = aggregate_inputs(expression, group)
      name = expression.value
      values = rows.map { |row| row[0] }
      nonnull = values.compact
      case name
      when 'count', 'count_star' then nonnull.length
      when 'sum', 'avg'
        return nil if nonnull.empty?
        if nonnull.first.is_a?(IntervalValue)
          sum = nonnull.reduce(IntervalValue.new) { |total, value| total + value }
          return name == 'sum' ? sum : sum * (1.0 / nonnull.length)
        end
        sum = cast_value(0, expression.type)
        nonnull.each do |value|
          value = cast_value(value, expression.type)
          decimal_aggregate_work(sum, value, :+) if value.is_a?(Decimal)
          sum = Types.binary('+', sum, value, type: expression.type)
        end
        name == 'sum' ? sum : Types.binary('/', sum, cast_value(nonnull.length, expression.type), type: expression.type)
      when 'min' then nonnull.min { |a, b| Types.compare(a, b) }
      when 'max' then nonnull.max { |a, b| Types.compare(a, b) }
      when 'bool_and', 'every' then nonnull.empty? ? nil : nonnull.all?
      when 'bool_or' then nonnull.empty? ? nil : nonnull.any?
      when 'bit_and' then nonnull.empty? ? nil : nonnull.reduce { |a, b| a & b }
      when 'bit_or' then nonnull.empty? ? nil : nonnull.reduce { |a, b| a | b }
      when 'bit_xor' then nonnull.empty? ? nil : nonnull.reduce { |a, b| a ^ b }
      when 'array_agg'
        return nil if values.empty?
        if expression.args.first.type.end_with?('[]')
          raise Error.new('22004', 'cannot accumulate null arrays') if values.any?(&:nil?)
          raise Error.new('2202E', 'cannot accumulate empty arrays') if values.any?(&:empty?)
          shape = values.first.dimensions
          bounds = values.first.lower_bounds
          raise Error.new('2202E', 'cannot accumulate arrays of differing dimensions') unless values.all? { |value| value.dimensions == shape && value.lower_bounds == bounds }
          ArrayValue.new(values, element_type: expression.args.first.type.delete_suffix('[]'))
        else
          cast_value(values, expression.type)
        end
      when 'string_agg'
        output = nil
        rows.each do |value, delimiter|
          next if value.nil?
          value = value.to_s if value.is_a?(Bytea)
          delimiter = delimiter.to_s if delimiter.is_a?(Bytea)
          output = output.nil? ? value.dup : output + (delimiter || '') + value
        end
        expression.type == 'bytea' && output ? Bytea.new(output) : output
      when *MOMENTS then aggregate_moment(name, nonnull, expression.type)
      when *PAIRS then aggregate_pair_moment(name, rows)
      when /\Ajsonb?_agg/ then aggregate_json_array(name, values, expression)
      when /\Ajsonb?_object_agg/ then aggregate_json_object(name, rows, expression)
      else raise Error.new('0A000', "aggregate #{name} is not implemented")
      end
    end

    def aggregate_inputs(expression, group)
      rows = []
      orderings = expression.extra[:aggregate_order] || []
      seen = {} if @query_memory && expression.extra[:distinct]
      group.each_with_index do |row, ordinal|
        next if expression.extra[:filter_expr] && evaluate(expression.extra[:filter_expr], row) != true
        charge
        values = expression.args.map { |arg| arg.kind == :star ? 1 : evaluate(arg, row) }
        charge if PAIRS.include?(expression.value) || expression.value == 'string_agg' || expression.value.include?('object_agg')
        keys = orderings.map { |compiled, _| evaluate(compiled, row) }
        if @query_memory && @query_memory_aggregate_owner
          owner = @query_memory_aggregate_owner
          if !orderings.empty?
            query_memory_add_row(owner, keys + values)
          end
          if seen && values.none?(&:nil?) && !seen.key?(values)
            query_memory_add(owner, MemorySchedule::ENTRY + query_memory_key_bytes(values))
            seen[values] = true
          end
          retained = expression.value.match?(/\A(?:array_agg|string_agg|jsonb?_agg|jsonb?_object_agg)/)
          retained &&= !values.first.nil? if expression.value == 'string_agg'
          retained &&= !values.last.nil? if expression.value.end_with?('_strict')
          query_memory_add(owner, query_memory_key_bytes(values, expression.args.map(&:type))) if retained
        end
        rows << [values, keys, ordinal]
      end
      rows.uniq! { |entry| entry[0] } if expression.extra[:distinct]
      unless orderings.empty?
        rows.sort! do |left, right|
          comparison = 0
          orderings.each_with_index do |(_, order), index|
            comparison = order_compare(left[1][index], right[1][index], order)
            break unless comparison.zero?
          end
          comparison.zero? ? left[2] <=> right[2] : comparison
        end
      end
      rows.map!(&:first)
    end

    def aggregate_json_array(name, values, expression)
      return nil if values.empty?
      binary = name.start_with?('jsonb')
      values = values.compact if name.end_with?('_strict')
      converted = values.map do |value|
        charge
        json_image_value(value, expression.args[0].type, expression.args[0], binary: binary)
      end
      data = converted.map(&:value)
      source = unless binary
        '[' + converted.each_with_index.map do |item, i|
          separator = i.zero? ? '' : values[i].is_a?(ArrayValue) || values[i].is_a?(CompositeValue) ? ", \n " : ', '
          separator + item.to_s
        end.join + ']'
      end
      JsonValue.new(data, binary: binary, source: source)
    end

    def aggregate_json_object(name, rows, expression)
      return nil if rows.empty?
      binary = name.start_with?('jsonb')
      strict = name.end_with?('_strict')
      unique = name.include?('_unique')
      data = {}
      pairs = []
      seen = {}
      rows.each do |key, value|
        raise Error.new(binary ? '22023' : '22004', 'JSON object key must not be null') if key.nil?
        raise Error.new('22023', 'JSON object key must be scalar') if key.is_a?(Array) || key.is_a?(JsonValue)
        key = json_key_text(key, expression.args[0].type)
        raise Error.new('22030', 'duplicate JSON object key') if unique && seen[key]
        seen[key] = true
        next if strict && value.nil?
        charge
        converted = json_image_value(value, expression.args[1].type, expression.args[1], binary: binary)
        data[key] = converted.value
        pairs << JsonValue.escape(key) + ' : ' + converted.to_s
      end
      JsonValue.new(data, binary: binary, source: binary ? nil : '{ ' + pairs.join(', ') + ' }')
    end

    def ordered_aggregate(expression, group)
      orders = expression.extra[:aggregate_order]
      rows = []
      group.each do |row|
        next if expression.extra[:filter_expr] && evaluate(expression.extra[:filter_expr], row) != true
        charge
        values = orders.map { |compiled, _| evaluate(compiled, row) }
        if @query_memory && @query_memory_aggregate_owner
          bytes = HYPOTHETICAL.include?(expression.value) ? query_memory_row_bytes(values) : values.first.nil? ? 0 : LogicalMemory.value_bytes(values.first, orders.first.first.type)
          query_memory_add(@query_memory_aggregate_owner, bytes)
          query_memory_add(@query_memory_aggregate_owner, query_memory_collation_bytes(values, @query_memory_aggregate_owner))
        end
        rows << values
      end
      rows.sort! do |left, right|
        comparison = 0
        orders.each_with_index do |(_, order), index|
          comparison = order_compare(left[index], right[index], order)
          break unless comparison.zero?
        end
        comparison
      end
      name = expression.value
      direct = expression.args.map { |arg| evaluate(arg, group.first || []) }
      if HYPOTHETICAL.include?(name)
        before = []
        equal = 0
        rows.each do |row|
          comparison = 0
          orders.each_with_index do |(_, order), index|
            comparison = order_compare(row[index], direct[index], order)
            break unless comparison.zero?
          end
          before << row if comparison.negative?
          equal += 1 if comparison.zero?
        end
        return case name
               when 'rank' then before.length + 1
               when 'dense_rank' then before.uniq.length + 1
               when 'percent_rank' then rows.empty? ? 0.0 : before.length.to_f / rows.length
               when 'cume_dist' then (before.length + equal + 1).to_f / (rows.length + 1)
               end
      end
      values = rows.map(&:first).compact
      if name == 'mode'
        return nil if values.empty?
        counts = {}
        values.each { |value| counts[value] = (counts[value] || 0) + 1 }
        return counts.max_by { |_, count| count }.first
      end
      fraction = direct.first
      if fraction.is_a?(Array)
        return nil if values.empty?
        items = fraction.map { |value| aggregate_percentile(name, value, values) }
        return ArrayValue.new(items, element_type: expression.type.delete_suffix('[]'), lower_bound: fraction.respond_to?(:lower_bound) ? fraction.lower_bound : 1)
      end
      aggregate_percentile(name, fraction, values)
    end

    def aggregate_percentile(name, fraction, values)
      return nil if fraction.nil?
      fraction = fraction.to_f
      raise Error.new('22003', 'percentile fraction must be between zero and one') if fraction.nan? || fraction < 0 || fraction > 1
      return nil if values.empty?
      return values[[(fraction * values.length).ceil - 1, 0].max] if name == 'percentile_disc'
      position = fraction * (values.length - 1)
      low, high = values[position.floor], values[position.ceil]
      return low if low.is_a?(IntervalValue) && position.floor == position.ceil
      if low.is_a?(IntervalValue)
        difference = high - low
        fraction = position - position.floor
        months = difference.months * fraction
        whole_months = months.to_i
        days = difference.days * fraction + (months - whole_months) * 30
        whole_days = days.to_i
        micros = (difference.micros * fraction + (days - whole_days) * TimestampValue::DAY).round(half: :even)
        return low + IntervalValue.new(whole_months, whole_days, micros)
      end
      low, high = low.to_f, high.to_f
      return low if position.floor == position.ceil
      low + (position - position.floor) * (high - low)
    end

    def aggregate_moment(name, values, type)
      sample = %w[var_samp variance stddev_samp stddev].include?(name)
      return nil if values.empty? || (sample && values.length < 2)
      stddev = name.start_with?('stddev')
      if type == 'decimal'
        sum = Decimal.new(0)
        square_sum = Decimal.new(0)
        values.each do |value|
          value = Decimal.parse(value)
          decimal_aggregate_work(value, value, :*)
          square = value * value
          decimal_aggregate_work(sum, value, :+)
          sum += value
          decimal_aggregate_work(square_sum, square, :+)
          square_sum += square
        end
        count = Decimal.new(values.length)
        numerator = count * square_sum - sum * sum
        return Decimal.new(0) unless numerator.positive?
        denominator = count * Decimal.new(sample ? values.length - 1 : values.length)
        variance = numerator / denominator
        return stddev ? aggregate_decimal_sqrt(variance, variance.scale) : variance
      end
      n = 0.0
      sum = 0.0
      squares = 0.0
      values.each do |number|
        number = number.to_f
        old_sum = sum
        next_n = n + 1.0
        sum += number
        if n.positive?
          delta = number * next_n - sum
          squares += delta * delta / (next_n * n)
          if sum.infinite? || squares.infinite?
            raise Error.new('22003', 'floating-point aggregate overflow') if old_sum.finite? && number.finite?
            squares = Float::NAN
          end
        elsif !number.finite?
          squares = Float::NAN
        end
        n = next_n
      end
      result = squares / (sample ? n - 1 : n)
      stddev ? Math.sqrt(result) : result
    end

    def aggregate_pair_moment(name, rows)
      count = 0.0
      sx = sy = sxx = syy = sxy = 0.0
      rows.each do |y, x|
        next if x.nil? || y.nil?
        x, y = x.to_f, y.to_f
        old_x, old_y = sx, sy
        n = count + 1.0
        sx += x
        sy += y
        if count.positive?
          tx = x * n - sx
          ty = y * n - sy
          factor = 1.0 / (n * count)
          sxx += tx * tx * factor
          syy += ty * ty * factor
          sxy += tx * ty * factor
          if sx.infinite? || sxx.infinite?
            raise Error.new('22003', 'floating-point aggregate overflow') if old_x.finite? && x.finite?
            sxx = Float::NAN
          end
          if sy.infinite? || syy.infinite?
            raise Error.new('22003', 'floating-point aggregate overflow') if old_y.finite? && y.finite?
            syy = Float::NAN
          end
          if sxy.infinite?
            raise Error.new('22003', 'floating-point aggregate overflow') if old_x.finite? && x.finite? && old_y.finite? && y.finite?
            sxy = Float::NAN
          end
        else
          sxx = sxy = Float::NAN unless x.finite?
          syy = sxy = Float::NAN unless y.finite?
        end
        count = n
      end
      return count.to_i if name == 'regr_count'
      return nil if count.zero?
      case name
      when 'covar_pop' then sxy / count
      when 'covar_samp' then count < 2 ? nil : sxy / (count - 1)
      when 'corr' then sxx.zero? || syy.zero? ? nil : sxy / Math.sqrt(sxx * syy)
      when 'regr_avgx' then sx / count
      when 'regr_avgy' then sy / count
      when 'regr_sxx' then sxx
      when 'regr_syy' then syy
      when 'regr_sxy' then sxy
      when 'regr_slope' then sxx.zero? ? nil : sxy / sxx
      when 'regr_intercept' then sxx.zero? ? nil : (sy - sx * sxy / sxx) / count
      when 'regr_r2' then sxx.zero? ? nil : syy.zero? ? 1.0 : (sxy * sxy) / (sxx * syy)
      end
    end

    def aggregate_decimal_sqrt(value, scale)
      power = 2 * scale - value.scale
      numerator = value.coefficient
      denominator = 1
      if power >= 0 then numerator *= 10**power else denominator = 10**-power end
      root = Integer.sqrt(numerator / denominator)
      root += 1 if 4 * numerator >= (2 * root + 1)**2 * denominator
      Decimal.new(root, scale)
    end

    def decimal_aggregate_work(left, right, operation)
      left_digits = left.zero? ? 0 : left.coefficient.abs.to_s.length
      right_digits = right.zero? ? 0 : right.coefficient.abs.to_s.length
      if operation == :+
        scale = [left.scale, right.scale].max
        left_digits += scale - left.scale
        right_digits += scale - right.scale
      end
      a = [1, (left_digits + 3) / 4].max
      b = [1, (right_digits + 3) / 4].max
      work = operation == :* ? a * b : [a, b].max
      charge(work - 1) if work > 1
    end

    def expression_name(node)
      return expression_name(node[:expr]) if node[:kind] == :subscript
      return 'grouping' if node[:kind] == :call && node[:name] == 'grouping'
      super
    end
  end

  Engine.prepend(AggregateExecution)
end
