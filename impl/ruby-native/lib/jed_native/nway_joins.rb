# frozen_string_literal: true

module JedNative
  # Deterministic left-deep search. A state retains physical and logical row
  # estimates separately: a bounded scan is not proof of predicate selectivity.
  module NWayJoins
    def materialize_from(sources, joins, predicate = nil)
      plan = @explain_runtime_plans && @explain_nway_plans&.[](@native_scan_statement&.object_id)
      return super unless plan || nway_join_shape?(sources, joins)
      plan ||= nway_join_plan(sources, joins, predicate, @native_scan_statement)
      bases = {}
      plan[:order].each_with_index do |ordinal, position|
        next if position.positive? && plan[:steps][position - 1][:algorithm] == :index
        bases[ordinal] = source_relation(plan[:sources][ordinal].merge(native_access_plan: plan[:accesses][position]))
      end
      first = bases.fetch(plan[:order].first)
      relation = Relation.new(columns: first.columns, rows: first.rows)
      plan[:steps].each_with_index do |step, index|
        ordinal = plan[:order][index + 1]
        source = plan[:sources][ordinal].merge(native_join_algorithm: step[:algorithm], native_index_plan: step[:index],
          native_on_trees: step[:ons], native_full_join_width: plan[:scope].length,
          native_original_on: join_conjunctions(step[:gates]), native_materialized: bases[ordinal])
        if plan[:ordered] && index == plan[:steps].length - 1
          source[:native_join_window] = { target: plan[:target], filter: predicate && compile_filter(predicate, relation.columns + plan[:scopes][ordinal]) }
        end
        start = @cost
        relation = join(relation, source, step[:kind], nil, nil)
        if step[:node]
          prefix_cost = index.zero? ? plan[:sources][plan[:order].first][:native_scan_profile].actual : plan[:steps][index - 1][:node].actual
          input_cost = step[:algorithm] == :index ? 0 : plan[:sources][ordinal][:native_scan_profile].actual
          step[:node].actual = prefix_cost + input_cost + @cost - start
        end
      end
      physical = plan[:order].flat_map { |ordinal| (plan[:offsets][ordinal]...(plan[:offsets][ordinal] + plan[:scopes][ordinal].length)).to_a }
      inverse = physical.each_with_index.to_h
      result = Relation.new(columns: plan[:scope], rows: relation.rows.map { |row| plan[:scope].each_index.map { |ordinal| row[inverse[ordinal]] } })
      query_memory_transfer(relation, result) if @query_memory
      [result, plan[:ordered] ? nil : predicate]
    ensure
      bases&.each_value { |relation| query_memory_release(relation) } if @query_memory
    end

    def nway_join_shape?(sources, joins)
      return false unless sources.length + joins.length > 2
      return false if sources.length > 1 && !joins.empty?
      return false if joins.any? { |edge| edge[:using] || edge[:natural] }
      (sources + joins.map { |edge| edge[:relation] }).all? do |source|
        source[:kind] == :table && !source[:lateral] && @tables.key?(source[:name]) && !@ctes.key?(source[:name])
      end
    end

    def nway_join_plan(sources, joins, where, statement)
      inputs = sources + joins.map { |edge| edge[:relation] }
      edges = sources.length > 1 ? Array.new(inputs.length - 1) { { kind: :cross } } : joins
      analyzer = ParameterBindings.new(self, nil)
      scopes = inputs.map { |source| analyzer.relation_columns(source) }
      scope = scopes.flatten
      offset = 0
      offsets = scopes.map { |columns| old = offset; offset += columns.length; old }
      predicates = edges.each_with_index.filter_map do |edge, index|
        next unless edge[:on]
        expression = compile_filter(edge[:on], scope)
        ordinals = expression_nodes(expression).select { |entry| entry.kind == :column }.map do |entry|
          offsets.each_index.find { |i| entry.value >= offsets[i] && entry.value < offsets[i] + scopes[i].length }
        end
        { owner: index + 1, dependencies: (ordinals + [index + 1]).uniq.reduce(0) { |mask, i| mask | (1 << i) },
          ast: edge[:on], gate: inputs[index + 1][:native_original_on] || edge[:on], kind: edge[:kind] }
      end
      access = inputs.each_with_index.map do |source, index|
        join_scan_candidates(source, scopes[index])
      end
      context = { sources: inputs, scopes: scopes, scope: scope, offsets: offsets, predicates: predicates,
                  available: access, where: where, statement: statement }
      prefix = nil
      start = 0
      while start < inputs.length
        if start.positive? && !%i[inner cross].include?(edges[start - 1][:kind])
          prefix = nway_extensions(context, prefix, start, fixed: true).min_by { |state| nway_state_key(state) }
          prefix[:ordered] = false
          start += 1
          next
        end
        finish = start + 1
        finish += 1 while finish < inputs.length && %i[inner cross].include?(edges[finish - 1][:kind])
        prefix = nway_island(context, (start...finish).to_a, prefix, final: finish == inputs.length)
        start = finish
      end
      context.merge(prefix)
    end

    private

    def nway_access_key(access)
      [AccessPlanning::ACCESS_RANK.fetch(access[:kind]), access[:name].to_s.b]
    end

    def nway_state_key(state)
      [state[:cost], state[:order], state[:accesses].map { |access| nway_access_key(access) }, state[:steps].map { |step| JoinPlanner::JOIN_RANK.fetch(step[:algorithm]) }]
    end

    def nway_driver(context, ordinal, access)
      { order: [ordinal], mask: 1 << ordinal, accesses: [access], steps: [], cost: access[:pipeline_cost],
        rows: access[:output_rows], logical: access[:logical_rows],
        ordered: join_driver_ordered?(context[:statement], context[:sources][ordinal], access) }
    end

    def nway_island(context, ordinals, prefix, final:)
      seeds = prefix ? [prefix] : ordinals.flat_map { |ordinal| context[:available][ordinal].map { |access| nway_driver(context, ordinal, access) } }
      if ordinals.length > 8
        state = seeds.min_by { |candidate| nway_state_key(candidate) }
        loop do
          remaining = ordinals.reject { |ordinal| state[:mask][ordinal] == 1 }
          break if remaining.empty?
          state = remaining.flat_map { |ordinal| nway_extensions(context, state, ordinal) }.min_by { |candidate| nway_state_key(candidate) }
        end
        return final ? nway_finish(context, state) : state
      end
      frontier = {}
      seeds.each { |seed| nway_retain(frontier, seed) }
      target = ordinals.reduce(prefix ? prefix[:mask] : 0) { |mask, ordinal| mask | (1 << ordinal) }
      ((prefix ? prefix[:order].length : 1)..(prefix ? prefix[:order].length + ordinals.length : ordinals.length)).each do |size|
        frontier.keys.sort.each do |bucket|
          states = frontier[bucket]
          next if states.first[:order].length != size || states.first[:mask] == target
          states.dup.each do |state|
            ordinals.each do |ordinal|
              next if state[:mask][ordinal] == 1
              nway_extensions(context, state, ordinal, frontier: frontier).each { |candidate| nway_retain(frontier, candidate) }
            end
          end
        end
      end
      complete = [false, true].flat_map { |ordered| frontier[[target, ordered ? 1 : 0]] || [] }
      complete.map! { |state| nway_finish(context, state) } if final
      complete.min_by { |state| nway_state_key(state) }
    end

    def nway_retain(frontier, candidate)
      bucket = [candidate[:mask], candidate[:ordered] ? 1 : 0]
      states = frontier[bucket] ||= []
      dimensions = %i[cost rows logical]
      return if states.any? do |prior|
        dimensions.all? { |key| prior[key] <= candidate[key] } &&
          (dimensions.any? { |key| prior[key] < candidate[key] } || (nway_state_key(prior) <=> nway_state_key(candidate)) <= 0)
      end
      states.reject! { |prior| dimensions.all? { |key| candidate[key] <= prior[key] } }
      states << candidate
      states.sort_by! { |state| nway_state_key(state) }
    end

    def nway_extension_inputs(context, state, ordinal, fixed:)
      mask = state[:mask] | (1 << ordinal)
      ready = context[:predicates].select { |entry| entry[:dependencies] & mask == entry[:dependencies] && entry[:dependencies] & state[:mask] != entry[:dependencies] }
      left_scope = state[:order].flat_map { |i| context[:scopes][i] }
      scope = left_scope + context[:scopes][ordinal]
      expressions = ready.map { |entry| compile_filter(entry[:ast], scope) }
      gates = ready.map { |entry| entry[:gate] }
      gate = join_conjunctions(gates)
      keys = gate && hash_join_keys(compile_filter(gate, scope), left_scope.length)
      kind = fixed ? ready.first&.dig(:kind) || :cross : ready.empty? ? :cross : :inner
      source = context[:sources][ordinal].merge(native_original_on: gate, native_join_where: context[:where])
      indexes = join_index_plan(Relation.new(columns: left_scope, rows: []), source, kind, join_conjunctions(ready.map { |entry| entry[:ast] }), nil, choose: false, all: !fixed)
      indexes = fixed ? [indexes].compact : indexes || []
      [scope, expressions, gates, keys, kind, indexes, ready.map { |entry| entry[:ast] }]
    end

    def nway_strictly_dominated?(states, cost, rows, logical)
      states.any? do |prior|
        prior[:cost] <= cost && prior[:rows] <= rows && prior[:logical] <= logical &&
          (prior[:cost] < cost || prior[:rows] < rows || prior[:logical] < logical)
      end
    end

    def nway_extensions(context, state, ordinal, fixed: false, frontier: nil)
      scope, expressions, gates, keys, kind, indexes, ons = nway_extension_inputs(context, state, ordinal, fixed: fixed)
      mask = state[:mask] | (1 << ordinal)
      ordered = state[:ordered] && !fixed
      prior_states = frontier[[mask, ordered ? 1 : 0]] if frontier && !@native_nway_frontier_disabled
      indexes = indexes.map do |index|
        index = index.merge(access: index[:access].dup)
        index[:access][:pipeline_cost] = index[:access][:scan_cost] + index[:access][:scan_rows] * expression_work(index[:filter])
        index
      end
      choices = context[:available][ordinal].flat_map do |access|
        algorithms = [:nested]
        algorithms.unshift(:hash) if keys && (!fixed || %i[left left_outer].include?(kind))
        algorithms.map { |algorithm| [access, algorithm, nil] }
      end
      choices = [] if fixed && !indexes.empty?
      indexes.each { |index| choices << [index[:access], :index, index] }
      choices.filter_map do |access, algorithm, bound|
        pairs = [state[:rows] * access[:output_rows], QueryPlans::MAX_ESTIMATE].min
        logical = algorithm == :index ? pairs : [state[:logical] * access[:logical_rows], QueryPlans::MAX_ESTIMATE].min
        rows = pairs
        unless algorithm == :index
          expressions.each do |expression|
            rows = estimate_join_rows(rows, expression, scope, context[:sources])
            logical = estimate_join_rows(logical, expression, scope, context[:sources])
          end
        end
        rows, logical = [rows, state[:rows]].max, [logical, state[:logical]].max if %i[left left_outer full full_outer].include?(kind)
        rows, logical = [rows, access[:output_rows]].max, [logical, access[:logical_rows] || access[:output_rows]].max if %i[right right_outer full full_outer].include?(kind)
        step = { algorithm: algorithm, index: bound, keys: keys, kind: kind, ons: ons, gates: gates,
                 work: expressions.sum { |expression| expression_work(expression) }, input_rows: state[:rows], rows: rows, logical: logical, pairs: pairs,
                 prefix_cost: state[:cost], access: access }
        step[:cost] = nway_step_cost(step, state[:rows], rows)
        # This bucket predates the whole extension batch. Keep all estimation
        # work in order; only omit state vectors that strict dominance makes
        # unnecessary. Exact triples still use the ordinary structural tie-break.
        next if prior_states && nway_strictly_dominated?(prior_states, step[:cost], rows, logical)
        { order: state[:order] + [ordinal], mask: mask, accesses: state[:accesses] + [access], steps: state[:steps] + [step],
          cost: step[:cost], rows: rows, logical: logical, ordered: ordered }
      end
    end

    def nway_step_cost(step, starts, delivered)
      access = step[:access]
      if step[:algorithm] == :index
        step[:prefix_cost] + starts * (access[:pipeline_cost] + access[:output_rows] * step[:work])
      elsif step[:algorithm] == :hash
        bytes = step[:keys].sum { |_, _, type| FileFormat::STORAGE_WIDTH.fetch(type, 1) }
        step[:prefix_cost] + access[:pipeline_cost] + (access[:output_rows] + starts) * bytes + delivered * (bytes + step[:keys].length * 4 + step[:work])
      else
        step[:prefix_cost] + access[:pipeline_cost] + starts * access[:output_rows] * step[:work]
      end.clamp(0, QueryPlans::MAX_ESTIMATE)
    end

    def nway_finish(context, state)
      state = state.merge(steps: state[:steps].map(&:dup))
      scope = state[:order].flat_map { |ordinal| context[:scopes][ordinal] }
      filter = context[:where] && compile_filter(context[:where], scope)
      output = [state[:rows], estimate_predicate(state[:logical], filter)].min
      if state[:ordered]
        target = join_window_target(context[:statement])
        step = state[:steps].last
        starts = target.zero? ? 0 : output > target ? (target * step[:input_rows] + output - 1) / output : step[:input_rows]
        delivered = step[:input_rows].zero? ? 0 : (starts * step[:rows] + step[:input_rows] - 1) / step[:input_rows]
        step[:cost] = nway_step_cost(step, starts, delivered)
        step[:rows], step[:calls] = delivered, starts
        state[:cost], state[:rows], state[:target] = step[:cost], delivered, target
        output = [output, target].min
      end
      state[:join_cost] = state[:cost]
      state[:output_rows] = output
      state[:cost] += state[:rows] * expression_work(filter) + output
      state
    end
  end
  Engine.prepend(NWayJoins)
end
