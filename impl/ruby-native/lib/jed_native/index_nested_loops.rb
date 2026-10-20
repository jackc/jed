# frozen_string_literal: true

module JedNative
  module IndexNestedLoops
    MAX_JOIN_BOUND_PARTS = 32
    MAX_JOIN_BOUND_NODES = 192
    MAX_JOIN_BOUND_UNITS = 4096
    JoinBoundStructure = Struct.new(:probes, :prepared, :terms, :keys, :comparisons,
      :empty_comparisons, :units, keyword_init: true)

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
        if !@native_join_bounds_disabled && (structure = plan[:native_join_bounds])
          structure.probes += 1
          prepare_join_bound_structure(plan, structure) if structure.probes == 2
        end
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
      structure = plan[:native_join_bounds] unless @native_join_bounds_disabled
      structure = nil unless structure&.prepared
      (plan[:access][:parts] || []).any? do |part|
        comparisons = if structure
          join_bound_comparisons(structure, structure.empty_comparisons, part, part[:key], part[:terms])
        else
          key_comparisons(part[:key], part[:terms])
        end
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

    private

    def prepare_join_index_execution(plan)
      return plan if @native_join_bounds_disabled || !join_bound_hash?(plan)
      access = plan[:access]
      return plan unless join_bound_hash?(access) && %i[primary secondary].include?(access[:kind])
      # Physical plans can be retained across executions. Only these private
      # shallow copies receive mutable probe state; no bound values are saved.
      structure = JoinBoundStructure.new(probes: 0, prepared: false, units: 0)
      plan.merge(native_join_bounds: structure, access: access.merge(native_join_bounds: structure))
    end

    def prepare_join_bound_structure(plan, structure)
      access = plan[:access]
      parts = access[:parts]
      return unless parts.instance_of?(Array) && parts.length <= MAX_JOIN_BOUND_PARTS
      stored = plan[:stored]
      return unless join_bound_hash?(stored)
      metadata = if access[:kind] == :primary
        stored[:native_primary_metadata]
      else
        return unless access[:name].instance_of?(String) && join_bound_hash?(stored[:native_secondary_metadata])
        stored[:native_secondary_metadata][access[:name]]
      end
      return unless join_bound_hash?(metadata)
      keys = access[:kind] == :primary ? metadata[:columns] : metadata[:expressions]
      types = metadata[:types]
      return unless keys.instance_of?(Array) && keys.length <= MAX_JOIN_BOUND_PARTS &&
        types.instance_of?(Array) && types.length == keys.length &&
        types.all? { |type| join_bound_type?(type) }
      roots = [plan[:predicate]]
      if access[:kind] == :primary
        return unless keys.all? { |key| key.instance_of?(Integer) }
      else
        roots.concat(keys)
      end
      parts.each do |part|
        return unless join_bound_hash?(part) && part[:terms].instance_of?(Array)
        return if roots.length + part[:terms].length + 1 > MAX_JOIN_BOUND_NODES
        roots << part[:key]
        roots.concat(part[:terms])
      end
      return unless join_bound_expressions?(roots)
      structure.keys = []
      structure.comparisons = []
      structure.empty_comparisons = {}.compare_by_identity
      structure.prepared = true
    end

    def join_bound_hash?(value)
      value.instance_of?(Hash) && value.default.nil? && !value.default_proc && !value.compare_by_identity?
    end

    def join_bound_type?(type)
      type.instance_of?(String) && %w[i16 i32 i64 boolean text decimal].include?(type) && !@types.key?(type)
    end

    def join_bound_expressions?(roots)
      pending = roots.map { |expression| [expression, 0] }
      count = 0
      until pending.empty?
        expression, depth = pending.pop
        count += 1
        return false if count > MAX_JOIN_BOUND_NODES || depth > 32
        return false unless expression.instance_of?(Expression) &&
          (join_bound_type?(expression.type) || %w[unknown null].include?(expression.type)) &&
          %i[literal column param outer unary binary cast numeric_context call].include?(expression.kind)
        extra = expression.extra
        if extra
          return false unless join_bound_hash?(extra)
          body_type, type_mod = extra[:native_body_type], extra[:type_mod]
          return false unless body_type.nil? || body_type.instance_of?(String)
          return false unless type_mod.nil? || type_mod.instance_of?(Array) && type_mod.length <= 2 &&
            type_mod.all? { |value| value.instance_of?(Integer) }
        end
        value = expression.value
        return false unless value.nil? || value.equal?(true) || value.equal?(false) ||
          value.instance_of?(Integer) || value.instance_of?(String) || value.instance_of?(Decimal) ||
          expression.kind == :outer && value.instance_of?(Array) && value.length == 2 && value.all? { |item| item.instance_of?(Integer) }
        args = expression.args
        next unless args
        return false unless args.instance_of?(Array) && count + pending.length + args.length <= MAX_JOIN_BOUND_NODES
        args.each { |child| pending << [child, depth + 1] }
      end
      true
    end

    def join_bound_terms(structure, predicate)
      return structure.terms if structure.terms
      terms = conjunctions(predicate)
      units = 1 + terms.length
      if structure.units + units <= MAX_JOIN_BOUND_UNITS
        structure.units += units
        structure.terms = terms.freeze
      end
      terms
    end

    def join_bound_comparisons(structure, cache, slot, key, terms)
      comparisons = cache[slot]
      return comparisons if comparisons
      comparisons = key_comparisons(key, terms)
      units = 2 + comparisons.length * 3
      if structure.units + units <= MAX_JOIN_BOUND_UNITS
        structure.units += units
        comparisons.each(&:freeze)
        cache[slot] = comparisons.freeze
      end
      comparisons
    end
  end
  Engine.prepend(IndexNestedLoops)
end
