# frozen_string_literal: true

module JedNative
  module IndexNestedLoops
    def join_index_plan(left, source, kind, on, using, choose: true, all: false)
      return unless %i[inner cross left left_outer].include?(kind) && source[:kind] == :table
      stored = @tables[source[:name]]
      return unless stored && !@ctes.key?(source[:name])
      columns = ParameterBindings.new(self, nil).relation_columns(source)
      scope = left.columns + columns
      if using
        names = using == :natural ? left.columns.reject { |c| c[:qualified_only] }.map { |c| c[:name] } & columns.map { |c| c[:name] } : using
        terms = names.map do |name|
          li = left.columns.each_index.select { |i| left.columns[i][:name] == name && !left.columns[i][:qualified_only] }
          ri = columns.each_index.select { |i| columns[i][:name] == name && !columns[i][:qualified_only] }
          return unless li.length == 1 && ri.length == 1
          { kind: :binary, op: '=', left: { kind: :column, native_ordinal: li.first }, right: { kind: :column, native_ordinal: left.columns.length + ri.first } }
        end
        on = join_conjunctions(terms)
      end
      original_on = source[:native_original_on] || on
      where = source[:native_join_where] if %i[inner cross].include?(kind)
      nodes = [original_on, where, source[:native_access_predicate]].compact.flat_map { |node| ast_conjunctions(node) }
      nodes.uniq!
      nodes.select! do |node|
        pushdown_column_nodes(node).all? do |column|
          column[:native_ordinal] || scope.any? { |entry| entry[:name] == column[:name] && (!column[:table] || entry[:table] == column[:table]) }
        end
      end
      return if nodes.empty?
      predicate = join_bound_expression(compile_filter(join_conjunctions(nodes), scope), left.columns.length)
      candidates = scan_access_candidates(stored, predicate).reject { |candidate| candidate[:kind] == :full }
      candidates.reject! { |candidate| candidate[:name] && stored[:indexes].any? { |index| index[:name] == candidate[:name] && index[:unparsed] } }
      candidates.select! do |candidate|
        bounds = candidate[:parts]&.flat_map { |part| part[:comparisons].map(&:last).compact } || candidate[:arms]&.map(&:last) || []
        bounds += candidate[:terms] || [] if %i[primary_interval secondary_interval].include?(candidate[:kind])
        bounds.any? { |bound| expression_nodes(bound).any? { |expression| expression.kind == :outer } }
      end
      return if candidates.empty?
      candidates.each { |candidate| estimate_scan_candidate(stored, predicate, candidate, nil) }
      local_filter = source[:native_pushed_filter] && compile_filter(source[:native_pushed_filter], columns)
      candidates.each { |candidate| candidate[:output_rows] = estimate_predicate(candidate[:scan_rows], local_filter) }
      if all
        return candidates.sort_by { |candidate| [AccessPlanning::ACCESS_RANK.fetch(candidate[:kind]), candidate[:name].to_s.b] }.map do |candidate|
          { stored: stored, columns: columns, predicate: predicate, access: candidate, filter: local_filter }
        end
      end
      selected = candidates.min_by { |candidate| [candidate[:pipeline_cost], AccessPlanning::ACCESS_RANK.fetch(candidate[:kind]), candidate[:name].to_s.b] }
      if choose && %i[inner cross].include?(kind)
        local = source[:native_access_predicate] && compile_filter(source[:native_access_predicate], columns)
        ordinary = scan_access_plan(stored, local)
        residual = on && compile_filter(on, scope)
        work = expression_work(residual)
        if source[:native_join_where]
          where = compile_filter(source[:native_join_where], scope)
          work += conjunctions(where).sum do |term|
            refs = expression_nodes(term).select { |entry| entry.kind == :column }.map(&:value)
            refs.any? { |index| index < left.columns.length } && refs.any? { |index| index >= left.columns.length } ? expression_work(term) : 0
          end
        end
        ordinary_cost = ordinary[:scan_cost] + left.rows.length * ordinary[:output_rows] * work
        if (keys = hash_join_keys(original_on && compile_filter(original_on, scope), left.columns.length))
          bytes = keys.sum { |_, _, type| FileFormat::STORAGE_WIDTH.fetch(type, 1) }
          pairs = keys.reduce(left.rows.length * ordinary[:output_rows]) { |rows, _| (rows + 199) / 200 }
          hash_cost = ordinary[:scan_cost] + (left.rows.length + ordinary[:output_rows]) * bytes + pairs * (bytes + 4 * keys.length + work)
          ordinary_cost = [ordinary_cost, hash_cost].min
        end
        inl_cost = left.rows.length * (selected[:scan_cost] + selected[:scan_rows] * work)
        return if inl_cost > ordinary_cost
      end
      { stored: stored, columns: columns, predicate: predicate, access: selected,
        filter: local_filter }
    end

    def join_bound_expression(expression, width)
      expression = expression.dup
      if expression.kind == :column
        if expression.value < width
          expression.kind, expression.value = :outer, [0, expression.value]
        else
          expression.value -= width
        end
      elsif expression.kind == :outer
        expression.value = [expression.value[0] + 1, expression.value[1]]
      end
      expression.args = expression.args.map { |arg| join_bound_expression(arg, width) } if expression.args
      expression
    end

    def join_index_relation(plan, left, row, spool: nil)
      correlated(left.columns, row) do
        scan_start = @cost if plan[:scan_profile]
        rows = join_empty_bound?(plan) ? [] : execute_scan_access(plan[:stored], plan[:predicate], plan[:access])
        unless rows
          charge(full_scan_pages(plan[:stored]))
          charge_scan_payload(plan[:stored])
          rows = plan[:stored][:native_primary_entries].map { |_, index| plan[:stored][:rows][index] }
        end
        if spool
          rows.each { |candidate| spool.push(candidate.to_a) }
          rows = spool
        elsif rows.is_a?(Enumerator)
          rows = rows.to_a
        end
        charge(rows.length)
        plan[:scan_profile].actual += @cost - scan_start if scan_start
        filter_start = @cost if plan[:filter_profile]
        if block_given?
          rows.each do |candidate|
            yield candidate if !plan[:filter] || evaluate(plan[:filter], candidate) == true
          end
        elsif plan[:filter]
          rows = rows.select { |candidate| evaluate(plan[:filter], candidate) == true }
        end
        if filter_start
          start = plan[:filter_profile].equal?(plan[:scan_profile]) ? filter_start : scan_start
          plan[:filter_profile].actual += @cost - start
        end
        next if block_given?
        relation = Relation.new(columns: plan[:columns], rows: rows)
        query_memory_relation(relation, mask: query_memory_mask(relation.columns)) if @query_memory
        relation
      end
    end

    def join_empty_bound?(plan)
      (plan[:access][:parts] || []).any? do |part|
        comparisons = key_comparisons(part[:key], part[:terms])
        equality = comparisons.find { |operator, _| operator == '=' }
        next false unless equality
        value = evaluate(equality[1], [])
        next true if value.nil?
        comparisons.any? do |operator, source|
          next operator == 'is null' unless source
          bound = evaluate(source, [])
          next true if bound.nil?
          comparison = Types.compare(value, bound)
          case operator
          when '=' then comparison != 0
          when '<' then comparison >= 0
          when '<=' then comparison > 0
          when '>' then comparison <= 0
          when '>=' then comparison < 0
          else false
          end
        end
      end
    end
  end
  Engine.prepend(IndexNestedLoops)
end
