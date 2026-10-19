# frozen_string_literal: true

module JedNative
  # The blocking file lane passes bounded, repeatable streams between logical
  # phases. A derived relation still owns its ordinary result buffer upstream.
  module SpillSelect
    FIXED_SPILL_AGGREGATES = %w[count sum avg min max bool_and bool_or every bit_and bit_or bit_xor].freeze
    COLLECTION_SPILL_AGGREGATES = %w[array_agg string_agg json_agg jsonb_agg json_agg_strict jsonb_agg_strict].freeze

    def select(statement)
      return super unless spill_select_shape?(statement)
      owners = []
      old_scan = [@native_stream_scan, @native_scan_columns]
      @native_stream_scan = true
      @native_scan_columns = scan_column_inventory(statement)
      sources = statement[:from] || []
      joins = statement[:joins] || []
      where = statement[:where]
      rewritten = @explain_rewritten_from&.[](statement.object_id) || rewrite_join_inputs(sources, joins, where)
      if rewritten
        sources, joins, where = rewritten
      end
      input_sources = sources + joins.map { |join| join[:relation] }
      scopes = input_sources.map { |source| spill_source_columns(source) }
      scope = scopes.flatten(1)
      projections = expand_stars(statement[:columns], scope)
      expressions = projections.map { |item| compile(item[:expr], scope) }
      columns = projections.zip(expressions).map { |item, expression| { name: item[:alias] || expression_name(item[:expr]), type: expression.type } }
      nodes = resolve_group_nodes(statement, scope)
      aggregate_nodes = spill_query_shape(statement)[1]
      grouped = !nodes.empty? || !aggregate_nodes.empty?
      validate_aggregate_scope(statement, scope, grouped)
      predicate = compile_filter(where, scope)
      having = compile_filter(statement[:having], scope)
      orderings = spill_order_expressions(statement, expressions, columns, scope)
      aggregates = aggregate_nodes.map { |node| compile(node, scope) }
      groupings = (expressions + [having] + orderings.map(&:first)).compact.flat_map { |expression| expression_nodes(expression) }.select { |expression| expression.kind == :grouping }.uniq { |expression| aggregate_expression_key(expression) }
      offset = statement[:offset] ? evaluate(compile(statement[:offset], []), []) || 0 : 0
      limit = statement[:limit] ? evaluate(compile(statement[:limit], []), []) : nil
      raise Error.new('2201X', 'negative OFFSET') if offset.negative?
      raise Error.new('2201W', 'negative LIMIT') if limit && limit.negative?
      contradiction = respond_to?(:where_contradiction?) && where_contradiction?(compile_filter(statement[:where], scope))
      if spill_stream_aggregate_plan(statement, sources, joins, nodes, aggregates, predicate, scope)
        return spill_stream_aggregate(sources.first, expressions, columns, aggregates, predicate, contradiction, offset, limit)
      end
      physical_plan = @explain_runtime_plans && @explain_join_plans&.[](statement.object_id)
      if physical_plan || two_base_join?(sources, joins)
        physical_plan ||= two_base_join_plan(sources.first, joins.first, where, statement)
        sources = [physical_plan[:outer]]
        inner = physical_plan[:inner].merge(native_join_algorithm: physical_plan[:algorithm], native_index_plan: physical_plan[:index])
        joins = [joins.first.merge(relation: inner)]
        input_sources = sources + [inner]
        scopes = input_sources.map { |source| spill_source_columns(source) }
      end
      index_plan = if physical_plan
        physical_plan[:index]
      elsif sources.length == 1 && joins.length == 1
        edge = joins.first
        edge[:relation][:native_index_plan] || join_index_plan(Relation.new(columns: scopes.first, rows: []), edge[:relation], edge[:kind], edge[:on], edge[:using])
      end
      inputs = input_sources.each_with_index.map do |source, index|
        next nil if index_plan && index == 1
        contradiction ? spill_owner(owners, RowSpool) : spill_source(source, scopes[index], statement, owners)
      end
      rows = inputs.first || spill_owner(owners, RowSpool)
      rows.push([]) if input_sources.empty? && !contradiction
      left_columns = scopes.first || []
      input_sources.drop(1).each_with_index do |_, relative|
        index = relative + 1
        join = index < sources.length ? { kind: :cross } : joins[index - sources.length]
        rows = if index_plan
          spill_index_join(rows, index_plan, left_columns, scopes[index], join, owners)
        else
          spill_join(rows, inputs[index], left_columns, scopes[index], join, owners)
        end
        left_columns += scopes[index]
      end
      if physical_plan && physical_plan[:reverse]
        reordered = spill_owner(owners, RowSpool)
        width = scopes.first.length
        rows.each { |row| reordered.push(row.drop(width) + row.take(width)) }
        rows.close
        rows = reordered
      end
      profile_checkpoint(:scan) if @explain_profile_frames
      filtered = spill_owner(owners, RowSpool)
      rows.each { |row| filtered.push(row) if !predicate || evaluate(predicate, row) == true }
      rows.close
      rows = filtered
      profile_checkpoint(:filter) if @explain_profile_frames
      if grouped
        rows, key_expressions = spill_groups(rows, scope, nodes, aggregates, owners, groupings)
        replacements = aggregates + groupings
        expressions = expressions.map { |expression| spill_rewrite(expression, key_expressions, replacements) }
        having = spill_rewrite(having, key_expressions, replacements) if having
        orderings = orderings.map { |expression, order| [spill_rewrite(expression, key_expressions, replacements), order] }
        if having
          kept = spill_owner(owners, RowSpool)
          rows.each { |row| kept.push(row) if evaluate(having, row) == true }
          rows.close
          rows = kept
        end
      end
      profile_checkpoint(:aggregate) if @explain_profile_frames
      unless orderings.empty?
        rows = spill_order(rows, orderings, owners)
      end
      profile_checkpoint(:sort) if @explain_profile_frames
      if statement[:distinct]
        seen, output = spill_owner(owners, SpillStateMap), spill_owner(owners, RowSpool)
        rows.each do |row|
          values = expressions.map { |expression| evaluate(expression, row) }
          output.push(values) if seen.insert(values)
        end
        rows.close
        seen.close
        rows = output
        expressions = columns.each_with_index.map { |column, index| Expression.new(kind: :column, type: column[:type], value: index) }
      end
      profile_checkpoint(:distinct) if @explain_profile_frames
      output = cursor_output(columns)
      output_owner = query_memory_owner if @query_memory && query_memory_collect_result?
      unless limit == 0
        window = rows.lazy.drop(offset)
        window = window.take(limit) if limit
        host_batch_rows(window, expressions, pulled: true).each do |row|
          values = expressions.map { |expression| evaluate(expression, row) }
          query_memory_add_row(output_owner, values, columns) if output_owner
          charge(0)
          @cost += 1
          @lifetime_cost += 1
          output << values
        end
      end
      profile_checkpoint(:root) if @explain_profile_frames
      result = Relation.new(columns: columns, rows: output)
      query_memory_attach(result, output_owner) if output_owner
      result
    ensure
      owners&.reverse_each(&:close)
      @native_stream_scan, @native_scan_columns = old_scan if old_scan
    end

    private

    # A plain whole-table fold needs no retained input rows. Keep budgeted and
    # profiled statements on the ordinary lane: their admission/abort schedule
    # is observable. Decimal and expression operands also retain that lane.
    def spill_stream_aggregate_plan(statement, sources, joins, nodes, aggregates, predicate, scope)
      return if @query_memory || @max_cost.positive? || @lifetime_max_cost.positive? || @explain_profile_frames
      return unless sources.length == 1 && joins.empty? && nodes.empty? && !aggregates.empty?
      return if statement[:distinct] || statement[:having] || !(statement[:order_by] || []).empty?
      source = sources.first
      return unless source[:kind] == :table && !@ctes.key?(source[:name]) && (stored = @tables[source[:name]])
      return if source[:native_pushed_filter]
      return unless aggregates.all? do |aggregate|
        extra = aggregate.extra
        next false if extra[:distinct] || extra[:filter_expr] || extra[:within_group] || !(extra[:aggregate_order] || []).empty?
        operand = aggregate.args.first
        next false unless aggregate.args.length == 1 && operand
        case aggregate.value
        when 'count' then %i[star column].include?(operand.kind)
        when 'min', 'max' then operand.kind == :column
        when 'sum' then operand.kind == :column && %w[i16 i32 f32 f64].include?(operand.type)
        when 'avg' then operand.kind == :column && %w[f32 f64].include?(operand.type)
        else false
        end
      end
      predicate = compile_filter(source[:native_access_predicate], scope) if source[:native_access_predicate]
      access = source[:native_access_plan] || scan_access_plan(stored, predicate, statement)
      # Bound scans keep their existing metering path. The full scan can defer
      # its page/row block until the walk succeeds, just like native folds.
      access if access[:kind] == :full
    end

    def spill_stream_aggregate(source, expressions, columns, aggregates, predicate, contradiction, offset, limit)
      stored = @tables.fetch(source[:name])
      states = aggregates.map { |aggregate| spill_accumulator(aggregate) }
      scanned = survivors = scan_units = 0
      unless contradiction
        scan_units = full_scan_pages(stored)
        scan_units += primary_scan_units(stored, nil, @native_scan_columns&.[](stored[:name])).sum if stored[:native_primary_metadata]
        ordinals = stored[:native_primary_entries]&.lazy&.map(&:last) || stored[:rows].each_index
        input = scan_rows(stored, ordinals)
        input.each do |row|
          scanned += 1
          next if predicate && evaluate(predicate, row) != true
          survivors += 1
          aggregates.each_with_index do |aggregate, index|
            operand = aggregate.args.first
            value = operand.kind == :star ? 1 : evaluate(operand, row)
            if aggregate.value == 'count'
              states[index][0] += 1 unless value.nil?
            else
              states[index] = spill_fold(aggregate, states[index], [value], nil, index)
            end
          end
        end
      end
      charge(scan_units + scanned + survivors * aggregates.length)
      row = aggregates.each_with_index.map { |aggregate, index| spill_finalize(aggregate, states[index], nil, index) }
      output = cursor_output(columns)
      if offset.zero? && limit != 0
        projections = expressions.map { |expression| spill_rewrite(expression, [], aggregates) }
        host_batch_rows([row], projections, pulled: true).each do |values|
          charge
          output << projections.map { |expression| evaluate(expression, values) }
        end
      end
      Relation.new(columns: columns, rows: output)
    end

    def spill_select_shape?(statement)
      return false unless spill_available? && statement[:kind] == :select && !statement[:with] && !statement[:distinct_on]
      has_window, aggregates = spill_query_shape(statement)
      return false if has_window || !(statement[:windows] || {}).empty?
      sources = (statement[:from] || []) + (statement[:joins] || []).map { |join| join[:relation] }
      return false unless sources.all? { |source| !source[:lateral] && %i[table subquery].include?(source[:kind]) && !source[:columns] && !source[:column_aliases] }
      if statement[:distinct] && streaming_select_shape?(statement) && sources.length == 1 && (stored = @tables[sources[0][:name]])
        scope = table_columns(stored, sources[0][:alias] || sources[0][:name])
        predicate = compile_filter(statement[:where], scope)
        return false unless scan_access_plan(stored, predicate, statement)[:order] == :blocking
      end
      return false if (statement[:joins] || []).any? { |join| join[:using] || join[:natural] || %i[right full right_outer full_outer].include?(join[:kind]) }
      return false unless aggregates.all? { |node| spill_aggregate_supported?(node) }
      return true if statement[:distinct] || !aggregates.empty? || !(statement[:group_by] || []).empty?
      joins = statement[:joins] || []
      return false if joins.empty?
      from = statement[:from] || []
      rewritten = rewrite_join_inputs(from, joins, statement[:where]) || [from, joins, statement[:where]]
      if two_base_join?(rewritten[0], rewritten[1])
        return two_base_join_plan(rewritten[0].first, rewritten[1].first, rewritten[2], statement)[:algorithm] == :hash
      end
      return false unless from.length == 1 && joins.length == 1
      left = spill_source_columns(from.first)
      right = spill_source_columns(joins.first[:relation])
      return false if join_index_plan(Relation.new(columns: left, rows: []), joins.first[:relation], joins.first[:kind], joins.first[:on], nil)
      !!hash_join_keys(compile_filter(joins.first[:on], left + right), left.length)
    end

    # These are syntax facts, independent of bindings, statistics, or the current
    # catalog. Key them by the retained statement, not a fresh wrapper Array on
    # every execution; those wrappers defeated the bounded window-shape cache.
    def spill_query_shape(statement)
      @native_spill_shapes ||= {}.compare_by_identity
      return @native_spill_shapes[statement] if @native_spill_shapes.key?(statement)
      shape = [contains_window?(statement[:columns]) || contains_window?(statement[:order_by]),
               projection_aggregates([statement[:columns], statement[:having], statement[:order_by]])]
      @native_spill_shapes.shift if @native_spill_shapes.length >= 64
      @native_spill_shapes[statement] = shape
    end

    def spill_owner(owners, klass, **options, &block)
      owner = klass.new(self, **options, &block)
      owners << owner
      owner
    end

    def spill_aggregate_supported?(node)
      (FIXED_SPILL_AGGREGATES + COLLECTION_SPILL_AGGREGATES).include?(node[:name]) && !node[:within_group] && (node[:order_by] || []).empty?
    end

    def spill_source_columns(source)
      if source[:kind] == :table && (stored = @tables[source[:name]]) && !@ctes.key?(source[:name])
        table_columns(stored, source[:alias] || source[:name])
      else
        ParameterBindings.new(self, {}).relation_columns(source)
      end
    end

    def spill_source(source, scope, statement, owners)
      output = spill_owner(owners, RowSpool)
      mask = spill_mask(statement, scope)
      if source[:kind] == :table && (stored = @tables[source[:name]]) && !@ctes.key?(source[:name])
        profile_start = @cost if source[:native_scan_profile]
        predicate = if source[:native_access_predicate]
          compile_filter(source[:native_access_predicate], scope)
        elsif (statement[:from] || []).length == 1 && (statement[:joins] || []).empty?
          compile_filter(statement[:where], scope)
        end
        pushed = compile_filter(source[:native_pushed_filter], scope)
        input = source[:native_access_plan] ? execute_scan_access(stored, predicate, source[:native_access_plan]) : primary_scan(stored, predicate)
        unless input
          charge(full_scan_pages(stored))
          charge_scan_payload(stored)
          ordinals = stored[:native_primary_entries]&.lazy&.map(&:last) || stored[:rows].each_index
          input = scan_rows(stored, ordinals)
        end
        input.each do |row|
          charge
          next if pushed && evaluate(pushed, row) != true
          output.push(mask.each_index.map { |index| mask[index] ? row[index] : nil })
        end
        source[:native_scan_profile].actual += @cost - profile_start if profile_start
      else
        relation = source_relation(source)
        query_memory_release(relation) if @query_memory
        relation.rows.each { |row| output.push(mask.each_index.map { |index| mask[index] ? row[index] : nil }) }
      end
      output
    end

    def spill_mask(statement, scope)
      touched = Array.new(scope.length, false)
      visit = lambda do |node|
        case node
        when Array then node.each { |child| visit.call(child) }
        when Hash
          if node[:kind] == :call && node[:name] == 'count' && node[:args]&.first&.dig(:kind) == :star
            visit.call(node[:filter])
          elsif %i[column star].include?(node[:kind])
            scope.each_with_index do |column, index|
              touched[index] = true if (!node[:table] || column[:table] == node[:table]) && (node[:kind] == :star || column[:name] == node[:name])
            end
          else
            node.each_value { |child| visit.call(child) }
          end
        end
      end
      visit.call(statement)
      touched
    end

    def spill_join(left, right, left_columns, right_columns, join, owners)
      output = spill_owner(owners, RowSpool)
      predicate = compile_filter(join[:on], left_columns + right_columns)
      gate = join[:relation]&.dig(:native_original_on)
      keys = hash_join_keys(gate ? compile_filter(gate, left_columns + right_columns) : predicate, left_columns.length)
      forced = join[:relation]&.dig(:native_join_algorithm) == :hash || join[:relation]&.dig(:native_fixed_hash)
      keys = nil if join[:relation]&.dig(:native_join_algorithm) == :nested
      if keys && !forced
        pairs = left.length * right.length
        width = keys.sum { |_, _, type| FileFormat::STORAGE_WIDTH.fetch(type, 1) }
        on_work = hash_join_operator_count(predicate)
        matches = keys.reduce(pairs) { |count, _| (count + 199) / 200 }
        keys = nil if !%i[left left_outer].include?(join[:kind]) && (left.length + right.length) * width + matches * (width + 4 * keys.length + on_work) > pairs * on_work
      end
      if keys
        build = spill_owner(owners, SpillHashRows)
        codec = statistics_codec
        right.each do |row|
          encoded = hash_join_row_key(row, keys, 1, codec)
          build.push(statistics_hash(encoded), [Bytea.new(encoded), CompositeValue.new(row)]) if encoded
        end
      end
      left.each do |left_row|
        matched = false
        matches = if keys
          encoded = hash_join_row_key(left_row, keys, 0, codec)
          spool = spill_owner(owners, RowSpool)
          if encoded
            build.each(statistics_hash(encoded)) do |stored_key, row|
              charge([[encoded.bytesize, stored_key.bytesize].min, 1].max)
              spool.push(row) if encoded == stored_key
            end
          end
          spool
        else
          right
        end
        matches.each do |right_row|
          row = left_row + right_row
          next if predicate && evaluate(predicate, row) != true
          matched = true
          output.push(row)
        end
        output.push(left_row + Array.new(right_columns.length)) if !matched && %i[left left_outer].include?(join[:kind])
        if keys
          matches.close
          owners.delete(matches)
        end
      end
      build&.close
      left.close
      right.close
      output
    end

    def spill_index_join(left, plan, left_columns, right_columns, join, owners)
      output = spill_owner(owners, RowSpool)
      predicate = compile_filter(join[:on], left_columns + right_columns)
      outer = Relation.new(columns: left_columns, rows: [])
      left.each do |row|
        matches = RowSpool.new(self)
        matched = false
        join_index_relation(plan, outer, row, spool: matches) do |inner|
          combined = row + inner.to_a
          next if predicate && evaluate(predicate, combined) != true
          matched = true
          output.push(combined)
        end
        if !matched && %i[left left_outer].include?(join[:kind])
          output.push(row + Array.new(right_columns.length))
        end
      ensure
        matches&.close
      end
      left.close
      output
    end

    def spill_order_expressions(statement, expressions, columns, scope)
      (statement[:order_by] || []).map do |order|
        node = order[:expr]
        expression = if node[:kind] == :literal && node[:value].is_a?(Integer)
          raise Error.new('42P10', 'ORDER BY position out of range') unless node[:value].between?(1, expressions.length)
          expressions[node[:value] - 1]
        elsif node[:kind] == :column && !node[:table] && (index = columns.index { |column| column[:name] == node[:name] })
          expressions[index]
        else
          compile(node, scope)
        end
        ensure_comparable_type(expression.type)
        [expression, order]
      end
    end

    def spill_order(rows, orderings, owners)
      decorated = spill_owner(owners, RowSpool)
      width = nil
      rows.each do |row|
        width ||= row.length
        decorated.push(row + orderings.map { |expression, _| evaluate(expression, row) })
      end
      rows.close
      width ||= 0
      compare = lambda do |left, right|
        result = 0
        orderings.each_with_index do |(_, order), index|
          result = order_compare(left[width + orderings.length + index], right[width + orderings.length + index], order)
          break unless result.zero?
        end
        result
      end
      sorter = spill_owner(owners, ExternalSorter, &compare)
      decorated.each do |row|
        keys = row.last(orderings.length).map do |value|
          spill_collation_sort_value(value)
        end
        sorter.push(row + keys)
      end
      decorated.close
      output = spill_owner(owners, RowSpool)
      sorter.each { |row| output.push(row) }
      sorter.close
      output
    end

    def spill_groups(rows, scope, nodes, aggregates, owners, groupings = [])
      sets = expand_grouping_sets(nodes)
      master = sets.flatten(1).uniq
      keys = master.map { |node| compile(node, scope) }
      output = spill_owner(owners, RowSpool)
      decorated = keys.each_index.select { |index| keys[index].kind != :column }
      unless decorated.empty?
        copy = spill_owner(owners, RowSpool)
        rows.each { |row| copy.push(row + decorated.map { |index| evaluate(keys[index], row) }) }
        rows.close
        rows = copy
      end
      sets.each do |set|
        groups, key_rows, seen, collections = spill_owner(owners, SpillStateMap), spill_owner(owners, RowSpool), spill_owner(owners, SpillStateMap), spill_owner(owners, SpillHashRows)
        unique = spill_owner(owners, SpillStateMap)
        key_indices = set.map { |node| master.index(node) }
        if set.empty?
          groups.put([], [0] + aggregates.map { |aggregate| spill_accumulator(aggregate) })
          key_rows.push([])
        end
        rows.each do |row|
          tuple = key_indices.map do |index|
            keys[index].kind == :column ? row[keys[index].value] : row[scope.length + decorated.index(index)]
          end
          found, state = groups.get(tuple)
          if found
            state = state.dup
          else
            state = [key_rows.length] + aggregates.map { |aggregate| spill_accumulator(aggregate) }
            key_rows.push(tuple)
          end
          aggregates.each_with_index do |aggregate, index|
            next if aggregate.extra[:filter_expr] && evaluate(aggregate.extra[:filter_expr], row) != true
            charge
            if aggregate.extra[:within_group]
              values = aggregate.extra[:aggregate_order].map { |argument, _| evaluate(argument, row) }
              next if !AggregateExecution::HYPOTHETICAL.include?(aggregate.value) && values.first.nil?
              values[0] = values[0].to_f if aggregate.value == 'percentile_cont' && !values[0].is_a?(IntervalValue)
              collections.push(state[0] * aggregates.length + index, values)
              next
            end
            values = aggregate.args.map { |argument| argument.kind == :star ? 1 : evaluate(argument, row) }
            charge if aggregate.value == 'string_agg' || aggregate.value.include?('object_agg') || AggregateExecution::PAIRS.include?(aggregate.value)
            orders = (aggregate.extra[:aggregate_order] || []).map { |argument, _| evaluate(argument, row) }
            if aggregate.extra[:distinct]
              next if values.first.nil? && !aggregate.value.match?(/\A(?:array_agg|string_agg|json)/)
              next unless seen.insert([state[0], index, *values])
            end
            unless orders.empty?
              collections.push(state[0] * aggregates.length + index, orders + values)
              next
            end
            state[index + 1] = spill_fold(aggregate, state[index + 1], values, collections, state[0] * aggregates.length + index, unique)
          end
          groups.put(tuple, state)
        end
        key_rows.each do |tuple|
          _, state = groups.get(tuple)
          values = master.each_index.map { |index| (position = key_indices.index(index)) ? tuple[position] : nil }
          values.concat(aggregates.each_with_index.map do |aggregate, index|
            if aggregate.extra[:within_group]
              aggregate = aggregate.dup
              aggregate.args = aggregate.args.map do |argument|
                Expression.new(kind: :literal, type: argument.type, value: evaluate(spill_rewrite(argument, keys, []), values))
              end
            end
            spill_finalize(aggregate, state[index + 1], collections, state[0] * aggregates.length + index)
          end)
          groupings.each do |grouping|
            values << grouping.args.reduce(0) do |mask, argument|
              index = keys.index { |key| aggregate_expression_key(key) == aggregate_expression_key(argument) }
              raise Error.new('42803', 'GROUPING argument must be a grouping expression') unless index
              (mask << 1) | (key_indices.include?(index) ? 0 : 1)
            end
          end
          output.push(values)
        end
        key_rows.close
        collections.close
        unique.close
        seen.close
        groups.close
      end
      rows.close
      [output, keys]
    end

    def spill_accumulator(expression)
      state = case expression.value
      when 'count' then [0]
      when 'sum' then [cast_value(0, expression.type), false]
      when 'avg' then [cast_value(0, expression.type), 0]
      when *COLLECTION_SPILL_AGGREGATES then [false]
      else [nil]
      end
      CompositeValue.new(state)
    end

    def spill_fold(expression, packed, values, collections, owner, unique = nil)
      state = packed.dup
      value = values.first
      name = expression.value
      case name
      when 'count' then state[0] += 1 unless value.nil?
      when 'sum', 'avg'
        unless value.nil?
          operand = cast_value(value, expression.type)
          decimal_aggregate_work(state[0], operand, :+) if operand.is_a?(Decimal)
          state[0] = Types.binary('+', state[0], operand, type: expression.type)
          state[1] = name == 'sum' ? true : state[1] + 1
        end
      when 'min', 'max'
        state[0] = value if !value.nil? && (state[0].nil? || (name == 'min' ? Types.compare(value, state[0]).negative? : Types.compare(value, state[0]).positive?))
      when 'bool_and', 'every', 'bool_or'
        state[0] = state[0].nil? ? value : name == 'bool_or' ? state[0] || value : state[0] && value unless value.nil?
      when 'bit_and', 'bit_or', 'bit_xor'
        state[0] = state[0].nil? ? value : state[0].public_send({ 'bit_and' => :&, 'bit_or' => :|, 'bit_xor' => :^ }.fetch(name), value) unless value.nil?
      else
        return packed if name == 'string_agg' && value.nil?
        return CompositeValue.new([true]) if name.end_with?('_strict') && value.nil?
        if name.start_with?('json')
          charge
          image = json_image_value(value, expression.args.first.type, expression.args.first, binary: name.start_with?('jsonb'))
          values = [JsonValue.new(image.value, binary: true, source: name.start_with?('jsonb') ? nil : image.to_s)]
        end
        collections.push(owner, values)
        state[0] = true
      end
      CompositeValue.new(state)
    end

    def spill_finalize(expression, state, collections, owner)
      name = expression.value
      case name
      when 'sum' then state[1] ? state[0] : nil
      when 'avg' then state[1].zero? ? nil : Types.binary('/', state[0], cast_value(state[1], expression.type), type: expression.type)
      when *FIXED_SPILL_AGGREGATES then state[0]
      when 'array_agg'
        return nil unless state[0]
        values = collections.each(owner).map(&:first)
        if expression.args.first.type.end_with?('[]')
          raise Error.new('22004', 'cannot accumulate null arrays') if values.any?(&:nil?)
          raise Error.new('2202E', 'cannot accumulate empty arrays') if values.any?(&:empty?)
          raise Error.new('2202E', 'cannot accumulate arrays of differing dimensions') unless values.all? { |value| value.dimensions == values.first.dimensions && value.lower_bounds == values.first.lower_bounds }
          ArrayValue.new(values, element_type: expression.args.first.type.delete_suffix('[]'))
        else
          cast_value(values, expression.type)
        end
      when 'string_agg'
        return nil unless state[0]
        output = nil
        collections.each(owner) { |value, delimiter| output ? output << (delimiter || '') << value : output = value.dup }
        expression.type == 'bytea' ? Bytea.new(output) : output
      else
        return nil unless state[0]
        values, fragments = [], []
        collections.each(owner) do |row|
          values << row.first.value
          fragments << row.first.to_s unless name.start_with?('jsonb')
        end
        source = unless name.start_with?('jsonb')
          type = expression.args.first.type
          composite = type.end_with?('[]') || @types.key?(type)
          '[' + fragments.each_with_index.map { |fragment, index| (index.zero? ? '' : composite && !values[index].nil? ? ", \n " : ', ') + fragment }.join + ']'
        end
        JsonValue.new(values, binary: name.start_with?('jsonb'), source: source)
      end
    end

    def spill_rewrite(expression, keys, aggregates)
      signature = aggregate_expression_key(expression)
      index = keys.index { |key| aggregate_expression_key(key) == signature }
      replacement = aggregates.index do |aggregate|
        if expression.extra.is_a?(Hash) && expression.extra[:aggregate_cache_id]
          aggregate.extra.is_a?(Hash) && aggregate.extra[:aggregate_cache_id] == expression.extra[:aggregate_cache_id]
        else
          aggregate_expression_key(aggregate) == signature
        end
      end
      index = keys.length + replacement if index.nil? && replacement
      return Expression.new(kind: :column, type: expression.type, value: index) if index
      copy = expression.dup
      copy.args = expression.args.map { |argument| argument.is_a?(Expression) ? spill_rewrite(argument, keys, aggregates) : argument } if expression.args
      copy
    end
  end
  Engine.prepend(SpillSelect)
end
