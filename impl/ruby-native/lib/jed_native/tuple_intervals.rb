# frozen_string_literal: true

module JedNative
  # A tuple bound is a product of encoded point prefixes, followed by at most
  # one range member. Keep member points separate until the entire tuple is
  # encoded: adjacent points in an earlier member have different suffixes.
  module TupleIntervals
    MAX_KEY_SET_PROBES = 65_536

    def scan_access_candidates(stored, predicate)
      candidates = super
      return candidates unless predicate && stored[:collation_skew].to_a.empty?
      primary = stored[:native_primary_metadata]
      if primary && primary[:columns].length > 1
        keys = primary[:columns].each_with_index.map { |column, i| Expression.new(kind: :column, type: primary[:types][i], value: column) }
        if (candidate = tuple_interval_candidate(predicate, keys))
          candidates << candidate.merge(kind: :primary_interval, unique: true, key_count: keys.length)
        end
      end
      (stored[:native_secondary_metadata] || {}).sort_by { |name, _| name.b }.each do |name, metadata|
        keys = metadata[:expressions]
        next unless keys.length == 1 || keys.all? { |key| key.kind == :column }
        next unless partial_predicate_implied?(metadata[:predicate], conjunctions(predicate))
        next unless (candidate = tuple_interval_candidate(predicate, keys))
        # Recovering the row-key suffix requires a self-delimiting scalar slot.
        next unless keys.all? { |key| tuple_scalar_key?(key.type) }
        candidates.reject! { |access| access[:kind] == :secondary_interval && access[:name] == name }
        candidates << candidate.merge(kind: :secondary_interval, name: name, partial: metadata[:predicate],
                                      unique: !!metadata[:definition][:unique], key_count: keys.length)
      end
      candidates
    end

    def plan_scan(stored, predicate, statement, access = nil)
      access ||= scan_access_plan(stored, predicate, statement)
      node = super(stored, predicate, statement, access)
      return node unless access[:tuple_members]
      members = access[:tuple_members]
      names = members.map { |member| stored[:columns][member[:key].value][:name] }.join(', ')
      if access[:kind] == :primary_interval
        node.detail = node.detail.sub(/PK interval set: [^;]+/, "PK interval set: #{names}")
      elsif members.length > 1
        node.detail = node.detail.sub("using #{access[:name]};", "using #{access[:name]} (#{names});")
      end
      node
    end

    private

    def estimate_array_elements(expression)
      return estimate_array_elements(expression.args.first) if expression.kind == :cast && expression.type.end_with?('[]')
      super
    end

    def estimate_predicate(rows, expression)
      if expression&.kind == :quantified
        source = expression.args[1]
        readable = %i[literal param outer].include?(source.kind) ||
          source.kind == :array && source.args.all? { |arg| %i[literal param outer].include?(arg.kind) } ||
          source.kind == :cast && source.type.end_with?('[]') && source.args.first.kind == :array && source.args.first.args.all? { |arg| %i[literal param outer].include?(arg.kind) }
        return (rows + 2) / 3 unless readable
      end
      super
    end

    def tuple_scalar_key?(type)
      FileFormat::STORAGE_WIDTH.key?(type) || %w[text bytea decimal interval].include?(type)
    end

    def tuple_interval_candidate(predicate, keys)
      terms = conjunctions(predicate)
      members = []
      keys.each do |key|
        break unless tuple_scalar_key?(key.type)
        interval = symbolic_interval_source(predicate, key)
        clips = terms.select { |term| !interval_set_source?(term) && !key_comparisons(key, [term]).empty? }
        break unless interval || !clips.empty?
        point = key_comparisons(key, clips).any? { |operator, _| operator == '=' } || interval && tuple_point_source?(interval[:source], key)
        members << { key: key, source: interval&.dig(:source), clips: clips, point: !!point,
                     intervals: interval ? interval[:intervals] : 0, arrays: interval ? interval[:arrays] : 0 }
        break unless point
      end
      return unless members.any? { |member| member[:source] }
      { tuple_members: members, key: members.first[:key], source: members.find { |member| member[:source] }[:source],
        intervals: members.sum { |member| member[:intervals] }, arrays: members.sum { |member| member[:arrays] },
        terms: members.flat_map { |member| [member[:source], *member[:clips]].compact } }
    end

    def tuple_point_source?(expression, key)
      if expression.kind == :binary && %w[and or].include?(expression.value)
        flags = expression.args.map { |part| tuple_point_source?(part, key) }
        return expression.value == 'or' ? flags.all? : flags.any?
      end
      return true if %i[quantified query_membership].include?(expression.kind)
      key_comparisons(key, [expression]).any? { |operator, _| operator == '=' }
    end

    def tuple_disjunct_count(expression, key)
      return 1 unless expression
      if expression.kind == :binary && %w[and or].include?(expression.value)
        counts = expression.args.map { |part| tuple_disjunct_count(part, key) }
        return counts.sum if expression.value == 'or'
        return 0 if counts.include?(0)
        return counts.max
      end
      if expression.kind == :quantified
        return estimate_array_elements(expression.args[1]).count { |element| !plan_constant(element).nil? }
      end
      if expression.kind == :query_membership
        rows = @native_estimate_membership ? nil : expression.extra[:rows]
        return rows ? rows.count { |row| !row.first.nil? } : 10
      end
      comparison = key_comparisons(key, [expression]).first
      comparison && comparison[1] && plan_constant(comparison[1]).nil? ? 0 : 1
    end

    def candidate_selectivity(count, access, stored)
      unless access[:tuple_members]
        return super unless %i[primary_interval secondary_interval].include?(access[:kind])
        rows = tuple_interval_selectivity(count, access[:source], access[:key], access[:unique])
        clips = access[:terms].reject { |term| term.equal?(access[:source]) }
        clip = clips.reduce { |a, b| Expression.new(kind: :binary, type: 'boolean', value: 'and', args: [a, b]) }
        return estimate_predicate(rows, clip)
      end
      rows, product, consumed, all_points = count, 1, 0, true
      access[:tuple_members].each do |member|
        quantity = tuple_disjunct_count(member[:source], member[:key])
        return 0 if quantity.zero?
        break if consumed.positive? && quantity > 1 && product * quantity > MAX_KEY_SET_PROBES
        product *= quantity
        consumed += 1
        all_points &&= member[:point]
        if member[:source]
          unique = access[:unique] && access[:key_count] == 1
          rows = tuple_interval_selectivity(rows, member[:source], member[:key], unique)
        end
        clip = member[:clips].reduce { |a, b| Expression.new(kind: :binary, type: 'boolean', value: 'and', args: [a, b]) }
        rows = estimate_predicate(rows, clip)
      end
      rows = [rows, product].min if access[:unique] && all_points && consumed == access[:key_count]
      if access[:partial]
        constrained = access[:tuple_members].take(consumed).map { |member| member[:key].value }
        conjunctions(access[:partial]).each do |term|
          columns = expression_nodes(term).select { |node| node.kind == :column }.map(&:value)
          rows = estimate_predicate(rows, term) if (columns & constrained).empty?
        end
      end
      rows
    end

    def tuple_interval_selectivity(count, source, key, unique)
      disjuncts = tuple_estimator_disjuncts(source, key)
      total = disjuncts.sum do |terms|
        next 0 if tuple_impossible_disjunct?(terms, key)
        predicate = terms.reduce { |a, b| Expression.new(kind: :binary, type: 'boolean', value: 'and', args: [a, b]) }
        estimate = estimate_predicate(count, predicate)
        estimate = [estimate, 1].min if unique && terms.all? { |term| term.kind == :binary && term.value == '=' }
        estimate
      end
      [count, total].min
    end

    def tuple_impossible_disjunct?(terms, key)
      comparisons = key_comparisons(key, terms)
      # Quantified promotion may produce a decimal operand against an integer
      # key. It remains an exact point source only when that value is integral.
      terms.each do |term|
        next unless term.kind == :binary && term.value == '=' && same_key_expression?(term.args[0], key)
        source = term.args[1]
        next unless Types::INTEGER_BITS.key?(key.type) && source.type == 'decimal'
        comparisons << ['=', source]
      end
      equality = comparisons.find { |operator, source| operator == '=' && plan_constant(source) != :unknown }
      return false unless equality
      value = plan_constant(equality[1])
      return true if value.nil?
      value = normalize_scan_bound(key.type, value)
      return true if value == :unmatchable
      if (bits = Types::INTEGER_BITS[key.type])
        return true unless value.is_a?(Integer) && value.between?(-(1 << (bits - 1)), (1 << (bits - 1)) - 1)
      end
      comparisons.any? do |operator, source|
        bound = plan_constant(source)
        next false if bound == :unknown
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

    def tuple_estimator_disjuncts(source, key)
      if source.kind == :binary && source.value == 'or'
        return source.args.flat_map { |part| tuple_estimator_disjuncts(part, key) }
      end
      if source.kind == :binary && source.value == 'and'
        left, right = source.args.map { |part| tuple_estimator_disjuncts(part, key) }
        return left.flat_map { |a| right.map { |b| a + b } }
      end
      elements = if source.kind == :quantified
        estimate_array_elements(source.args[1])
      elsif source.kind == :query_membership
        rows = @native_estimate_membership ? nil : source.extra[:rows]
        rows ? rows.map { |row| Expression.new(kind: :literal, type: key.type, value: row.first) } : Array.new(10) { Expression.new(kind: :outer, type: key.type) }
      end
      return [[source]] unless elements
      elements.map { |element| [Expression.new(kind: :binary, type: 'boolean', value: '=', args: [key, element])] }
    end

    def tuple_member_encoder(stored, name, position)
      metadata = name ? stored[:native_secondary_metadata][name] : stored[:native_primary_metadata]
      codec = name ? stored[:native_secondary_codec] : stored[:native_primary_codec]
      type, collation = metadata[:types][position], metadata[:collations][position]
      lambda do |value|
        bytes = codec.key_body(type, statistics_value(type, value), collation)
        bytes
      end
    end

    def tuple_source_points(expression, key, encoder)
      if expression.kind == :binary && expression.value == 'or'
        return expression.args.flat_map { |part| tuple_source_points(part, key, encoder) }
      end
      if expression.kind == :binary && expression.value == 'and'
        point, clip = expression.args
        point, clip = clip, point unless tuple_point_source?(point, key)
        points = tuple_source_points(point, key, encoder)
        intervals = reduce_intervals(clip, key, encoder)
        return points.select { |bytes| intervals.any? { |lower, upper| (!lower || bytes >= lower) && (!upper || bytes < upper) } }
      end
      if expression.kind == :quantified
        source = expression.args[1]
        values = source.kind == :array ? source.args.map { |argument| evaluate(argument, []) } : evaluate(source, []) || []
        return values.flat_map { |value| point_interval(value, key.type, encoder).map(&:first) }
      end
      if expression.kind == :query_membership
        return expression.extra[:rows].flat_map { |row| point_interval(row.first, key.type, encoder).map(&:first) }
      end
      comparison = key_comparisons(key, [expression]).find { |operator, _| operator == '=' }
      comparison ? point_interval(evaluate(comparison[1], []), key.type, encoder).map(&:first) : []
    end

    def tuple_member_intervals(member, encoder, order)
      key, source, clips = member.values_at(:key, :source, :clips)
      if member[:point]
        equality = clips.find { |clip| key_comparisons(key, [clip]).any? { |operator, _| operator == '=' } }
        points = tuple_source_points(equality || source, key, encoder).uniq.sort
        restrictions = equality ? [source, *clips].compact : clips
        restrictions.each do |expression|
          intervals = reduce_intervals(expression, key, encoder)
          points.select! { |bytes| intervals.any? { |lower, upper| (!lower || bytes >= lower) && (!upper || bytes < upper) } }
        end
        if order
          descending, nulls_first = order
          tag = [descending ? (nulls_first ? 255 : 254) : (nulls_first ? 1 : 0)].pack('C')
          points.map! { |point| tag + (descending ? tuple_reverse_bytes(point) : point) }
          points.sort!
        end
        return points.map { |point| [point, prefix_successor(point)] }
      end
      intervals = source ? reduce_intervals(source, key, encoder) : [[nil, nil]]
      clips.each { |clip| intervals = intersect_intervals(intervals, reduce_intervals(clip, key, encoder)) }
      if order
        descending, nulls_first = order
        tag = [descending ? (nulls_first ? 255 : 254) : (nulls_first ? 1 : 0)].pack('C')
        intervals = intervals.map do |lower, upper|
          if descending
            [upper ? prefix_successor(tag + tuple_reverse_bytes(upper)) : tag,
             lower ? prefix_successor(tag + tuple_reverse_bytes(lower)) : prefix_successor(tag)]
          else
            [lower ? tag + lower : tag, upper ? tag + upper : prefix_successor(tag)]
          end
        end
        intervals = merge_intervals(intervals)
      end
      intervals
    end

    def tuple_reverse_bytes(bytes)
      bytes.bytes.map { |byte| byte ^ 255 }.pack('C*')
    end

    def tuple_scan_intervals(stored, access)
      prefixes, result, product = [''.b], nil, 1
      access[:tuple_members].each_with_index do |member, position|
        if expression_nodes(member[:source]).any? { |term| term.kind == :query_membership && term.extra[:native_unfolded] }
          break
        end
        encoder = tuple_member_encoder(stored, access[:name], position)
        order = access[:name] && stored[:native_secondary_metadata][access[:name]][:orders][position]
        intervals = tuple_member_intervals(member, encoder, order)
        return [] if intervals.empty?
        break if position.positive? && intervals.length > 1 && product * intervals.length > MAX_KEY_SET_PROBES
        product *= intervals.length
        if member[:point]
          prefixes = prefixes.flat_map { |prefix| intervals.map { |lower, _| prefix + lower } }
        else
          result = prefixes.flat_map do |prefix|
            intervals.map do |lower, upper|
              [lower ? prefix + lower : prefix.empty? ? nil : prefix,
               upper ? prefix + upper : prefix_successor(prefix)]
            end
          end
          break
        end
      end
      result ||= prefixes.map { |prefix| [prefix.empty? ? nil : prefix, prefix_successor(prefix)] }
      merge_intervals(result)
    end

    def execute_interval_access(stored, predicate, access)
      return super unless access[:tuple_members]
      name = access[:name]
      intervals = tuple_scan_intervals(stored, access)
      intervals.reverse! if @native_reverse_scan
      iterator = Enumerator.new do |output|
        intervals.each do |lower, upper|
          if name
            charge(secondary_page_count(stored, name, lower, upper))
            storage_scan_entries(stored, name, lower: lower, upper: upper, reverse: !!@native_reverse_scan).each do |_, ordinal|
              charge(stored.dig(:native_primary_metadata, :height) || 1)
              charge_scan_payload(stored, [ordinal])
              output << storage_row(stored, ordinal)
            end
          else
            storage_stream_rows(stored, lower: lower, upper: upper, reverse: !!@native_reverse_scan).each { |row| output << row }
          end
        end
      end
      @native_stream_scan ? iterator : iterator.to_a
    end
  end
  Engine.prepend(TupleIntervals)
end
