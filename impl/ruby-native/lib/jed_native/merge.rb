# frozen_string_literal: true

module JedNative
  # MERGE (spec/design/merge.md): one buffered source query, a per-source-row
  # target probe planned from the join condition with the source as outer
  # constants, an ordered action list validated against the end state, then
  # RETURNING and the writes. Expressions read the pre-statement table.
  module MergeSyntax
    def merge_statement
      expect('merge')
      expect('into')
      table, database = qualified_name
      table_alias = nil
      if accept('as')
        table_alias = identifier
      elsif alias_available?(Parser::RELATION_STOPS)
        table_alias = identifier
      end
      expect('using')
      from = [table_reference]
      joins = join_clauses.map { |join| join.merge(from_index: 0) }
      expect('on')
      condition = expression
      clauses, unconditional = [], {}
      while accept('when')
        clause = {}
        if accept('not')
          expect('matched')
          clause[:match] = accept('by') && (accept('source') ? :by_source : (expect('target'); :not_matched)) || :not_matched
        else
          expect('matched')
          clause[:match] = :matched
        end
        clause[:condition] = expression if accept('and')
        expect('then')
        if accept('do')
          expect('nothing')
          clause[:action] = :nothing
        elsif clause[:match] != :not_matched && accept('update')
          expect('set')
          clause[:action] = :update
          clause[:assignments] = assignments
        elsif clause[:match] != :not_matched && accept('delete')
          clause[:action] = :delete
        elsif clause[:match] == :not_matched && accept('insert')
          clause[:action] = :insert
          clause[:columns] = name_list if at?('(')
          if accept('overriding')
            clause[:overriding] = identifier
            syntax('expected SYSTEM or USER') unless %w[system user].include?(clause[:overriding])
            expect('value')
          end
          if accept('default')
            expect('values')
            clause[:default_values] = true
          else
            expect('values')
            expect('(')
            clause[:values] = separated { expression }
            expect(')')
          end
        else
          syntax('invalid action for this WHEN clause')
        end
        if unconditional[clause[:match]]
          raise Error.new('42601', 'unreachable WHEN clause specified after unconditional WHEN clause')
        end
        unconditional[clause[:match]] = true unless clause[:condition]
        clauses << clause
      end
      syntax('MERGE requires at least one WHEN clause') if clauses.empty?
      result = { kind: :merge, table: table, database: database, alias: table_alias,
                 source: { from: from, joins: joins }, on: condition, clauses: clauses }
      parse_returning(result)
      result
    end
  end
  Parser.include(MergeSyntax)

  module MergeBindings
    private

    def statement(node, outer = [])
      return super unless node.is_a?(Hash) && node[:kind] == :merge
      previous_ctes, previous_depth = @ctes.dup, @scope_depth
      @scope_depth += 1
      (node[:with] || []).each do |cte|
        columns = statement(cte[:query], outer)
        @ctes[cte[:name]] = columns.each_with_index.map { |column, i| column.merge(name: cte[:columns]&.[](i) || column[:name]) }
      end
      view = @engine.instance_variable_get(:@views)[node[:table]]
      table = view ? { name: node[:table], columns: @engine.send(:view_schema, view) } : @engine.table(node[:table])
      label = node[:alias] || node[:table]
      target = @engine.table_columns(table, label).map { |column| column.merge(binding_scope: @scope_depth) }
      source = input_columns(node[:source], outer).map { |column| column.merge(binding_scope: @scope_depth) }
      expression(node[:on], target + source + outer, 'boolean')
      node[:clauses].each do |clause|
        scope = case clause[:match]
                when :matched then target + source
                when :by_source then target
                else source
                end + outer
        expression(clause[:condition], scope, 'boolean') if clause[:condition]
        (clause[:assignments] || []).each do |item|
          assignment(item[:expr], table[:columns].find { |column| column[:name] == item[:column] }, scope)
        end
        next unless clause[:values]
        columns = clause[:columns] ? clause[:columns].map { |name| table[:columns].find { |column| column[:name] == name } } : table[:columns]
        clause[:values].each_with_index { |value, i| assignment(value, columns[i], scope) if columns[i] }
      end
      aliases = node[:returning_aliases] || {}
      versions = %w[old new].flat_map do |name|
        @engine.table_columns(table, aliases[name.to_sym] || name).map { |column| column.merge(binding_scope: @scope_depth, qualified_only: true) }
      end
      returning_scope = source + target + versions + [{ name: '?merge_action?', type: 'text', qualified_only: true, native_merge_action: true }] + outer
      (node[:returning] || []).flat_map do |item|
        expr = item[:expr]
        if expr[:kind] == :star
          returning_scope.select { |column| expr[:table] ? column[:table] == expr[:table] : !column[:qualified_only] && column[:binding_scope] == @scope_depth }
        else
          [{ name: item[:alias] || @engine.expression_name(expr), type: expression(expr, returning_scope) }]
        end
      end
    ensure
      if node.is_a?(Hash) && node[:kind] == :merge
        @ctes = previous_ctes
        @scope_depth = previous_depth
      end
    end

    def resolve_expression(node, scope, wanted = nil)
      if node.is_a?(Hash) && node[:kind] == :call && node[:name].to_s.downcase == 'merge_action' && (node[:args] || []).empty?
        raise Error.new('42601', 'MERGE_ACTION() can only be used in the RETURNING list of a MERGE command') unless scope.any? { |column| column[:native_merge_action] }
        return 'text'
      end
      super
    end
  end
  ParameterBindings.prepend(MergeBindings)

  module Merge
    MERGE_ACTION_COLUMN = { name: '?merge_action?', type: 'text', qualified_only: true, native_merge_action: true }.freeze

    def compile(node, columns)
      if node.is_a?(Hash) && node[:kind] == :call && node[:name].to_s.downcase == 'merge_action' && (node[:args] || []).empty?
        index = columns.index { |column| column[:native_merge_action] }
        return Expression.new(kind: :column, type: 'text', value: index) if index
        @outer.reverse_each.with_index do |outer, depth|
          position = outer[0].index { |column| column[:native_merge_action] }
          return Expression.new(kind: :outer, type: 'text', value: [depth, position]) if position
        end
        raise Error.new('42601', 'MERGE_ACTION() can only be used in the RETURNING list of a MERGE command')
      end
      super
    end

    def merge(statement)
      return with_cte_mutation(statement) { merge(statement) } if @cte_read_tables && !@cte_mutation
      before = statistics_row_snapshots
      plan = @explain_runtime_plans && @explain_runtime_plans[statement.object_id]
      previous_profile = @native_merge_profile
      @native_merge_profile = plan&.instance_variable_get(:@native_merge_profile)
      start = @cost
      result = merge_rows(*merge_target_statement(statement))
      plan.actual = @cost - start if plan
      mark_changed_statistics(before) if result.rows_affected.positive?
      result
    ensure
      @native_merge_profile = previous_profile
    end

    def build_query_plan(statement)
      return super unless statement[:kind] == :merge && (statement[:with] || []).empty?
      node = plan_merge(statement)
      plan_nodes(node).each do |child|
        child.instance_variable_set(:@native_plan_owner, statement.object_id) unless child.instance_variable_defined?(:@native_plan_owner)
        child.actual ||= 0
      end
      @explain_plans[statement.object_id] = node
      node
    end

    private

    def authorize_node(node, ctes, tables, seen_views)
      return super unless node.is_a?(Hash) && node[:kind] == :merge
      name = node[:table]
      ctes = ctes.dup
      (node[:with] || []).each do |cte|
        authorize_node(cte[:query], ctes, tables, seen_views)
        ctes[cte[:name]] = true
      end
      if tables && (@tables.key?(name) || @views.key?(name))
        require_privilege(name, :select)
        node[:clauses].each do |clause|
          case clause[:action]
          when :update
            authorize_mutation({ kind: :update, table: name, alias: node[:alias], assignments: clause[:assignments] }, ctes, tables, seen_views)
          when :delete
            authorize_mutation({ kind: :delete, table: name, alias: node[:alias] }, ctes, tables, seen_views)
          when :insert
            authorize_mutation({ kind: :insert, table: name, alias: node[:alias], columns: merge_insert_names(node, clause),
                                 rows: clause[:values] ? [clause[:values]] : nil, default_values: clause[:default_values] }, ctes, tables, seen_views)
            view = @views[name]
            names = merge_insert_names(node, clause)
            (view && view[:defaults] || {}).each do |column, expression|
              authorize_node(expression, ctes, false, seen_views) unless names.include?(column)
            end
          end
        end
      end
      node.each do |key, child|
        next if key == :with
        authorize_node(child, ctes, tables, seen_views)
      end
    end

    def merge_insert_names(statement, clause)
      return clause[:columns] if clause[:columns]
      return [] if clause[:default_values]
      view = @views[statement[:table]]
      columns = view ? view_schema(view) : (@tables[statement[:table]] || table(statement[:table]))[:columns]
      columns.first(clause[:values].length).map { |column| column[:name] }
    end


    # Retarget a statement into a view at the view's base table (views.md
    # §15.8): each clause is validated as a write of its own kind, the view
    # WHERE chain leads the join condition and filters the not-matched-by-
    # source scan, and the check options apply to every placed row.
    def merge_target_statement(statement)
      return [statement, nil] unless @views.key?(statement[:table])
      view = @views[statement[:table]]
      top = view_write_plan(view, :delete)
      label = statement[:alias] || statement[:table]
      aliases = { old: 'old', new: 'new' }.merge(statement[:returning_aliases] || {})
      qualifiers = [label, *aliases.values]
      local = merge_source_columns(statement).map { |column| column[:name] }
      rewritten = statement.merge(table: top[:table], alias: label)
      rewritten[:clauses] = statement[:clauses].map do |clause|
        clause = clause.dup
        visible = clause[:match] == :by_source ? [] : local
        clause[:condition] = rewrite_view_value(clause[:condition], top[:columns], qualifiers, label, visible) if clause[:condition] && clause[:match] != :not_matched
        case clause[:action]
        when :delete then view_write_plan(view, :delete)
        when :update
          plan = view_write_plan(view, :update)
          names = clause[:assignments].map { |assignment| assignment[:column] }
          raise Error.new('42701', 'duplicate target column') unless names.uniq.length == names.length
          clause[:assignments] = clause[:assignments].map do |assignment|
            target = writable_view_column(plan, assignment[:column])
            node = assignment[:expr]
            node = if node[:kind] == :default
                     view[:defaults]&.[](assignment[:column]) || plan[:defaults][assignment[:column]] || { kind: :default }
                   else
                     rewrite_view_value(node, plan[:columns], qualifiers, label, visible)
                   end
            assignment.merge(column: target, expr: node)
          end
          raise Error.new('42701', 'duplicate target column') unless clause[:assignments].map { |a| a[:column] }.uniq.length == names.length
        when :insert
          plan = view_write_plan(view, :insert)
          insert_defaults = plan[:defaults].merge(view[:defaults] || {})
          names = merge_insert_names(statement, clause)
          raise Error.new('42701', 'duplicate target column') unless names.uniq.length == names.length
          if clause[:values] && clause[:values].length > names.length
            raise Error.new('42601', 'INSERT has more expressions than target columns')
          end
          columns = names.map { |name| writable_view_column(plan, name) }
          values = (clause[:values] || []).each_with_index.map { |node, index| node[:kind] == :default && insert_defaults.key?(names[index]) ? insert_defaults[names[index]] : node }
          extra = insert_defaults.keys - names
          if clause[:default_values] && extra.empty?
            clause[:columns] = nil
          else
            clause[:columns] = columns + extra.map { |name| writable_view_column(plan, name) }
            clause[:values] = values + extra.map { |name| insert_defaults[name] }
            clause[:default_values] = false
          end
        end
        clause
      end
      rewritten[:on] = conjunction(relabel_view_targets(top[:predicate], label), rewrite_view_value(statement[:on], top[:columns], qualifiers, label, local))
      if statement[:returning]
        projections = statement[:returning].flat_map do |item|
          node = item[:expr]
          if node[:kind] == :star && (!node[:table] || qualifiers.include?(node[:table]))
            columns = view_schema(view).map { |column| { expr: { kind: :column, name: column[:name], table: node[:table] || label }, alias: column[:name] } }
            node[:table] ? columns : [{ expr: { kind: :star, native_merge_source: true } }] + columns
          else
            [item.merge(alias: item[:alias] || expression_name(node))]
          end
        end
        rewritten[:returning] = projections.map do |item|
          next item if item[:expr][:native_merge_source]
          item.merge(expr: rewrite_view_value(item[:expr], top[:columns], qualifiers, label, local))
        end
      end
      base_columns = table_columns(table(top[:table]), label)
      context = {
        filter: top[:predicate] && relabel_view_targets(top[:predicate], label),
        checks: top[:checks].map { |node| compile_source_order_filter(relabel_view_targets(node, label), base_columns) }
      }
      [rewritten, context]
    end

    def merge_source_query(statement, extra = [])
      source = statement[:source]
      { kind: :select, columns: [{ expr: { kind: :star } }] + extra, from: source[:from], joins: source[:joins] || [],
        distinct: false, group_by: [], order_by: [], windows: {} }
    end

    def merge_source_columns(statement)
      query = merge_source_query(statement)
      query_input_columns(query, @ctes.transform_values(&:columns), [], ParameterBindings.new(self, query))
    end

    def merge_source_label(statement)
      source = statement[:source]
      return nil unless (source[:joins] || []).empty? && source[:from].length == 1
      item = source[:from].first
      return item[:alias] if item[:alias]
      case item[:kind]
      when :table then item[:name]
      when :function then item.dig(:expr, :name)
      end
    end

    def merge_window?(value)
      return value.any? { |child| merge_window?(child) } if value.is_a?(Array)
      return false unless value.is_a?(Hash)
      return false if %i[subquery exists array_query].include?(value[:kind])
      return true if value[:kind] == :call && value[:over]
      value.values.any? { |child| merge_window?(child) }
    end

    def merge_expression_check(node)
      raise Error.new('42803', 'aggregate functions are not allowed in MERGE') if local_aggregate?(node)
      raise Error.new('42P20', 'window functions are not allowed in MERGE') if merge_window?(node)
    end

    def merge_identity(row) = row.is_a?(PagedRow) ? row.storage_key : row.object_id

    def merge_resolve_insert(t, clause, scope)
      columns = t[:columns]
      if clause[:default_values]
        return { provided: Array.new(columns.length), defaults: merge_insert_defaults(t, Array.new(columns.length)) }
      end
      values = clause[:values]
      targets = clause[:columns] ? mutation_target_indices(t, clause[:columns]) : (0...columns.length).to_a
      raise Error.new('42601', 'INSERT has more expressions than target columns') if values.length > targets.length
      raise Error.new('42601', 'INSERT has more target columns than expressions') if clause[:columns] && values.length < targets.length
      provided = Array.new(columns.length)
      values.each_with_index do |node, position|
        merge_expression_check(node)
        ordinal = targets[position]
        column = columns[ordinal]
        next if node[:kind] == :default
        next if column[:identity] && clause[:overriding] == 'user'
        if column[:identity] == :always && clause[:overriding] != 'system'
          raise Error.new('428C9', "cannot insert a non-DEFAULT value into column #{column[:name]}")
        end
        provided[ordinal] = compile(node, scope)
      end
      { provided: provided, defaults: merge_insert_defaults(t, provided) }
    end

    def merge_insert_defaults(t, provided)
      t[:columns].each_with_index.map do |column, ordinal|
        provided[ordinal] || !column[:default] ? nil : compile(column[:default], [])
      end
    end

    def merge_build_insert(t, plan, source_row, checks)
      assignment_columns = t[:columns].map { |column| column.merge(nullable: true) }
      candidate = Array.new(t[:columns].length)
      context = MutationFrame.new([source_row])
      plan[:provided].each_with_index do |expression, ordinal|
        next unless expression
        candidate[ordinal] = assign(evaluate(expression, context), assignment_columns[ordinal], expression)
      end
      plan[:defaults].each_with_index do |expression, ordinal|
        next if plan[:provided][ordinal]
        value = expression && assignment_value(expression)
        candidate[ordinal] = assign(value, assignment_columns[ordinal], expression)
      end
      validate_candidate(t, candidate, checks)
      candidate
    end

    def merge_target_rows(t, predicate)
      candidates = primary_scan(t, predicate)
      unless candidates
        charge(full_scan_pages(t))
        charge_scan_payload(t)
      end
      candidates || t[:rows]
    end

    # Resolve everything the matching pass and EXPLAIN share (merge.md §2,
    # §3): scopes, the source query, the join condition in its two forms,
    # the clauses, RETURNING, the touched set and the matching loop's form.
    def merge_prepare(statement, view_context)
      t = table(statement[:table])
      label = statement[:alias] || statement[:table]
      source_label = merge_source_label(statement)
      raise Error.new('42712', "table name \"#{label}\" specified more than once") if source_label && source_label == label
      raw_source = merge_source_columns(statement)
      visible = raw_source.reject { |column| column[:qualified_only] || column[:star_hidden] }
      hidden = raw_source.select { |column| column[:qualified_only] && !column[:star_hidden] }
      source_scope = visible + hidden
      query = merge_source_query(statement, hidden.map { |column| { expr: { kind: :column, table: column[:table], name: column[:name] } } })
      target_scope = table_columns(t, label)
      width, source_width = target_scope.length, source_scope.length
      both = target_scope + source_scope
      merge_expression_check(statement[:on])
      on = compile_filter(statement[:on], both)
      probe = correlated(source_scope, nil) { compile_filter(statement[:on], target_scope) }
      clauses = statement[:clauses].map do |clause|
        scope = case clause[:match]
                when :matched then both
                when :by_source then target_scope
                else source_scope
                end
        merge_expression_check(clause[:condition])
        condition = compile_source_order_filter(clause[:condition], scope)
        plan = { match: clause[:match], action: clause[:action], condition: condition }
        case clause[:action]
        when :update
          (clause[:assignments] || []).each { |assignment| merge_expression_check(assignment[:expr]) }
          plan[:assignments] = resolve_assignments(t, clause[:assignments], scope)
        when :insert
          plan[:insert] = merge_resolve_insert(t, clause, scope)
        end
        plan
      end
      return_scope, return_exprs, return_columns = merge_returning(statement, t, label, source_label, source_scope)
      filter = view_context && view_context[:filter] && compile_filter(view_context[:filter], target_scope)
      touched = merge_touched(width, on, clauses, return_exprs, source_width)
      access = correlated(source_scope, Array.new(source_width)) { scan_access_plan(t, probe, statement) }
      prepared = { t: t, label: label, source_scope: source_scope, query: query, width: width, source_width: source_width,
                   on: on, probe: probe, clauses: clauses, filter: filter, touched: touched, access: access,
                   return_scope: return_scope, return_exprs: return_exprs, return_columns: return_columns,
                   by_source: clauses.any? { |clause| clause[:match] == :by_source } }
      keys = access[:kind] == :full && hash_join_keys(on, width)
      prepared[:hash] = keys if keys && merge_hash_estimate(prepared, keys, statement)
      prepared
    end

    # estimator.md §9.4: the two loop estimates over the same source plan.
    def merge_loop_estimates(prepared, source, statement)
      t, access = prepared[:t], prepared[:access]
      rows = source.rows
      saturate = ->(value) { [value, QueryPlans::MAX_ESTIMATE].min }
      nested = saturate.call(source.cost + rows * access[:pipeline_cost])
      keys = prepared[:hash] || (access[:kind] == :full && hash_join_keys(prepared[:on], prepared[:width]))
      return [nested, nil] unless keys
      full = estimate_scan_candidate(t, nil, { kind: :full, terms: [] }, statement)
      bytes = keys.sum { |_, _, type| FileFormat::STORAGE_WIDTH.fetch(type, 1) }
      matches = saturate.call(rows * access[:output_rows])
      hashed = saturate.call(source.cost + full[:scan_cost] + full[:scan_rows] * bytes + rows * bytes +
                             matches * (4 * keys.length + bytes) + hash_join_operator_count(prepared[:on]) * matches)
      [nested, hashed]
    end

    def merge_hash_estimate(prepared, keys, statement)
      source = merge_source_plan(prepared[:query])
      return false unless source
      nested, hashed = merge_loop_estimates(prepared.merge(hash: keys), source, statement)
      hashed < nested
    end

    # Plan the source query without running it; plain execution needs its
    # estimate only to choose the matching loop's form. Planning is unmetered.
    MERGE_PLAN_STATE = %i[@explain_options @explain_plans @explain_runtime_plans @explain_rewritten_from @explain_group_components
                          @explain_join_plans @explain_nway_plans @explain_cte_references @explain_cte_definitions @explain_cte_main
                          @cost @lifetime_cost].freeze

    def merge_source_plan(query)
      saved = MERGE_PLAN_STATE.map { |name| instance_variable_get(name) }
      @explain_options ||= { verbose: false, costs: true, analyze: false }
      @explain_plans, @explain_runtime_plans = {}, nil
      @explain_rewritten_from, @explain_group_components, @explain_join_plans, @explain_nway_plans = {}, {}, {}, {}
      @explain_cte_references, @explain_cte_definitions, @explain_cte_main = [], {}, {}
      build_query_plan(plan_copy_query(query))
    rescue Error
      nil
    ensure
      MERGE_PLAN_STATE.each_with_index { |name, index| instance_variable_set(name, saved[index]) }
    end

    def merge_hash_build(t, rows, keys)
      codec = statistics_codec
      buckets = {}
      owner = query_memory_owner if @query_memory
      build_keys = keys.map { |target, source, type| [source, target, type] }
      rows.each_with_index do |row, index|
        bytes = hash_join_row_key(row, build_keys, 1, codec)
        next unless bytes
        if owner
          size = keys.sum(MemorySchedule::ENTRY) { |ordinal, _, type| LogicalMemory.value_bytes(row[ordinal], type) }
          query_memory_add(owner, size)
        end
        (buckets[statistics_hash(bytes)] ||= []) << [bytes, index]
      end
      [build_keys, codec, buckets, owner]
    end

    def merge_rows(statement, view_context)
      previous_columns, previous_statement = @native_scan_columns, @native_scan_statement
      prepared = merge_prepare(statement, view_context)
      profile = @native_merge_profile
      t, on, probe, clauses, filter = prepared.values_at(:t, :on, :probe, :clauses, :filter)
      source_scope = prepared[:source_scope]
      checks = mutation_checks(t)
      @native_scan_columns = (@native_scan_columns || {}).merge(t[:name] => prepared[:touched])
      @native_scan_statement = statement
      start = @cost
      source_rows = select(profile ? profile[:query] : prepared[:query]).rows.to_a
      loop_cost = @cost - start
      probe_cost = 0
      actions, affected, matched = [], {}, {}
      by_source = prepared[:by_source]
      choose = lambda do |match, context|
        clauses.find { |clause| clause[:match] == match && (!clause[:condition] || evaluate(clause[:condition], context) == true) }
      end
      plan_action = lambda do |clause, row, context, source_row|
        return unless clause && clause[:action] != :nothing
        identity = merge_identity(row)
        raise Error.new('21000', 'MERGE command cannot affect row a second time') if affected[identity]
        affected[identity] = true
        if clause[:action] == :delete
          actions << [:delete, row, nil, source_row]
        else
          replacement = apply_assignments(t, row, context, clause[:assignments], retain: true)
          validate_candidate(t, replacement, checks)
          actions << [:update, row, replacement, source_row]
        end
      end
      pair = lambda do |row, source_row|
        mark = @cost
        context = MutationFrame.new([row, source_row])
        accepted = evaluate(on, context) == true
        loop_cost += @cost - mark
        next false unless accepted
        matched[merge_identity(row)] = true if by_source
        plan_action.call(choose.call(:matched, context), row, context, source_row)
        true
      end
      if (keys = prepared[:hash])
        mark = @cost
        target_rows = merge_target_rows(t, nil).map { |row| charge; row }
        probe_cost = @cost - mark
        table = merge_hash_build(t, target_rows, keys)
        loop_cost += @cost - mark
      end
      source_rows.each do |source_row|
        found = false
        if keys
          mark = @cost
          candidates = hash_join_candidates(table, source_row).map { |index| target_rows[index] }
          loop_cost += @cost - mark
          candidates.each { |row| found = true if pair.call(row, source_row) }
        else
          mark = @cost
          candidates = correlated(source_scope, source_row) { merge_target_rows(t, probe) }
          probe_cost += @cost - mark
          loop_cost += @cost - mark
          candidates.each do |row|
            charge
            probe_cost += 1
            loop_cost += 1
            found = true if pair.call(row, source_row)
          end
        end
        next if found
        clause = choose.call(:not_matched, source_row)
        next unless clause && clause[:action] == :insert
        actions << [:insert, nil, merge_build_insert(t, clause[:insert], source_row, checks), source_row]
      end
      hash_join_release(table) if keys
      if profile
        profile[:loop].actual += loop_cost
        profile[:probe].actual += probe_cost
      end
      if by_source
        mark = @cost
        target_rows = merge_target_rows(t, filter)
        scan_cost, filter_cost = @cost - mark, 0
        target_rows.each do |row|
          charge
          scan_cost += 1
          if filter
            mark = @cost
            accepted = evaluate(filter, row) == true
            filter_cost += @cost - mark
            next unless accepted
          end
          next if matched[merge_identity(row)]
          plan_action.call(choose.call(:by_source, row), row, row, nil)
        end
        if profile
          profile[:scan].actual += scan_cost
          profile[:filter].actual += scan_cost + filter_cost if profile[:filter]
        end
      end
      merge_apply(statement, prepared, actions, view_context)
    ensure
      @native_scan_columns, @native_scan_statement = previous_columns, previous_statement
    end

    def merge_touched(width, on, clauses, return_exprs, source_width)
      ordinals = []
      visit = lambda do |expression, limit, modulo|
        expression_nodes(expression).each do |node|
          next unless node.kind == :column && node.value.is_a?(Integer)
          index = node.value
          if modulo
            next if index < source_width || index >= source_width + 3 * width
            ordinals << (index - source_width) % width
          elsif index < limit
            ordinals << index
          end
        end
      end
      visit.call(on, width, false)
      clauses.each do |clause|
        next if clause[:match] == :not_matched
        visit.call(clause[:condition], width, false) if clause[:condition]
        (clause[:assignments] || []).each { |_, expression| visit.call(expression, width, false) }
      end
      (return_exprs || []).each { |expression| visit.call(expression, 0, true) }
      ordinals.uniq.sort
    end

    def merge_returning(statement, t, label, source_label, source_scope)
      return [nil, nil, nil] unless statement[:returning]
      explicit = statement[:returning_aliases] || {}
      taken = [label, source_label].compact
      aliases = { old: 'old', new: 'new' }.reject { |version, name| !explicit.key?(version) && (explicit.value?(name) || taken.include?(name)) }.merge(explicit)
      raise Error.new('42712', 'duplicate RETURNING alias') if explicit.values.uniq.length != explicit.length || explicit.values.include?(label)
      target = table_columns(t, label)
      scope = source_scope + target
      %i[old new].each do |version|
        name = aliases[version]
        columns = table_columns(t, name || "?#{version}?").map { |column| column.merge(qualified_only: true, row_version: version) }
        scope.concat(columns)
      end
      scope << MERGE_ACTION_COLUMN
      items = statement[:returning].flat_map do |item|
        next [item] unless item[:expr][:native_merge_source]
        source_scope.each_index.reject { |i| source_scope[i][:qualified_only] }.map do |i|
          { expr: { kind: :column, table: source_scope[i][:table], name: source_scope[i][:name], native_ordinal: i }, alias: source_scope[i][:name] }
        end
      end
      projections = expand_stars(items, scope)
      expressions = projections.map { |projection| merge_expression_check(projection[:expr]) || compile(projection[:expr], scope) }
      columns = projections.zip(expressions).map { |p, e| { name: p[:alias] || expression_name(p[:expr]), type: e.type } }
      [scope, expressions, columns]
    end

    def merge_apply(statement, prepared, actions, view_context)
      t, return_exprs, return_columns, source_width = prepared.values_at(:t, :return_exprs, :return_columns, :source_width)
      paged = t[:rows].is_a?(PagedRows) || t[:rows].is_a?(PendingRows)
      next_id = t[:next_rowid]
      if paged
        rows = t[:rows].is_a?(PendingRows) ? t[:rows].dup : PendingRows.new(t)
        actions.each do |kind, old_row, new_row, _|
          case kind
          when :insert
            rows.append(new_row, next_id)
            next_id += 1
          else rows.replace(old_row, new_row)
          end
        end
        rowids = rows.rowids
      else
        positions = t[:rows].each_with_index.to_h { |row, index| [row.object_id, index] }
        current, ids, removed = t[:rows].dup, t[:rowids].dup, {}
        appended, appended_ids = [], []
        actions.each do |kind, old_row, new_row, _|
          case kind
          when :insert
            appended << new_row
            appended_ids << next_id
            next_id += 1
          when :update then current[positions.fetch(old_row.object_id)] = new_row
          when :delete then removed[positions.fetch(old_row.object_id)] = true
          end
        end
        rows, rowids = [], []
        current.each_with_index do |row, index|
          next if removed[index]
          rows << row
          rowids << ids[index]
        end
        rows.concat(appended)
        rowids.concat(appended_ids)
      end
      validate_rows(t.merge(constraints: t[:constraints].reject { |constraint| constraint[:kind] == :check }), rows)
      if view_context
        actions.each do |_, _, new_row, _|
          next unless new_row
          view_context[:checks].each do |predicate|
            raise Error.new('44000', 'new row violates check option for view') unless evaluate(predicate, new_row) == true
          end
        end
      end
      changes = actions.map { |_, old_row, new_row, _| [old_row, new_row] }
      result = if return_exprs
        width = t[:columns].length
        null_target, null_source = Array.new(width), Array.new(source_width)
        memory_owner = query_memory_owner if @query_memory
        output = actions.map do |kind, old_row, new_row, source_row|
          current = kind == :delete ? old_row : new_row
          context = MutationFrame.new([source_row || null_source, current, old_row || null_target, new_row || null_target, [kind.to_s.upcase]])
          values = return_exprs.map { |expression| evaluate(expression, context) }
          charge
          query_memory_add_row(memory_owner, values, return_columns) if @query_memory
          values
        end
        Result.new(columns: return_columns.map { |c| c[:name] }, column_types: return_columns.map { |c| c[:type] }, rows: output, rows_affected: actions.length)
      else
        Result.new(rows_affected: actions.length)
      end
      project_mutation(statement, t, :merge, changes, nil, nil, nil)
      unless actions.empty?
        @tables[t[:name]] = t.merge(rows: rows, rowids: rowids, next_rowid: next_id)
        enforce_references(t, actions.select { |kind, *| kind == :delete }.map { |_, old_row, *| [old_row, nil] }, :delete)
        enforce_references(t, actions.select { |kind, *| kind == :update }.map { |_, old_row, new_row, _| [old_row, new_row] }, :update)
        @dirty = true
      end
      result
    end

    # EXPLAIN MERGE (explain.md §5, estimator.md §9.4): the Merge root over
    # the matching loop and, with NOT MATCHED BY SOURCE, the full target scan.
    def plan_merge(original)
      statement, view_context = merge_target_statement(original)
      prepared = merge_prepare(statement, view_context)
      t, access, on = prepared.values_at(:t, :access, :on)
      source = build_query_plan(prepared[:query])
      rows = source.rows
      saturate = ->(value) { [value, QueryPlans::MAX_ESTIMATE].min }
      touched = prepared[:touched].empty? ? '' : "; touched=#{prepared[:touched].length}"
      conjuncts = @explain_options[:verbose] ? "on=#{plan_expression(on)}" : "on:conjuncts=#{conjunctions(on).length}"
      nested, hashed = merge_loop_estimates(prepared, source, statement)
      if (keys = prepared[:hash])
        full = estimate_scan_candidate(t, nil, { kind: :full, terms: [] }, statement)
        scan = plan_scan(t, nil, statement, full)
        scan.detail += touched
        loop = plan_node('Hash Join', "left; keys=#{keys.length}; #{conjuncts}", saturate.call(rows * access[:output_rows]), hashed, [source, scan])
      else
        scan = correlated(prepared[:source_scope], Array.new(prepared[:source_width])) { plan_scan(t, prepared[:probe], statement, access) }
        scan.detail = scan.detail.gsub(/outer\([^)]*\)/, 'outer') + touched
        scan.rows = saturate.call(scan.rows * rows)
        scan.cost = saturate.call(scan.cost * rows)
        loop = plan_node('Nested Loop', "left; #{conjuncts}", saturate.call(rows * access[:output_rows]), nested, [source, scan])
      end
      scan.phase = nil
      children = [loop]
      profile = { query: prepared[:query], loop: loop, probe: scan }
      if prepared[:by_source]
        filter = prepared[:filter]
        full = estimate_scan_candidate(t, filter, scan_access_plan(t, filter, statement), statement)
        tail = plan_scan(t, filter, statement, full)
        tail.detail += touched
        tail.phase = nil
        profile[:scan] = tail
        if filter
          detail = @explain_options[:verbose] ? "filter=#{plan_expression(filter)}" : "conjuncts=#{conjunctions(filter).length}"
          tail = plan_node('Filter', detail, [tail.rows, estimate_predicate(t[:rows].length, filter)].min, saturate.call(tail.cost + tail.rows * expression_work(filter)), [tail])
          profile[:filter] = tail
        end
        children << tail
      end
      by_source = children[1]
      root = plan_node("Merge #{t[:local_name] || t[:name]}", "when=#{original[:clauses].length}",
                       saturate.call(rows + (by_source ? by_source.rows : 0)), saturate.call(loop.cost + (by_source ? by_source.cost : 0)), children)
      plan_nodes(root).each { |node| node.actual ||= 0 }
      root.instance_variable_set(:@native_merge_profile, profile)
      root
    end
  end
  Engine.prepend(Merge)
end
