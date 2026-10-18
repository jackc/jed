# frozen_string_literal: true

module JedNative
  module AuxiliaryScans
    def primary_scan(stored, predicate)
      result = super
      return result unless result.nil?
      bound = auxiliary_scan_bound(stored, predicate)
      return nil unless bound
      execute_auxiliary_scan(stored, bound)
    end

    def execute_auxiliary_scan(stored, bound)
      name, kind, arms = bound.values_at(:name, :kind, :arms)
      kind = kind.to_s
      if kind == 'gist'
        metadata = stored[:native_auxiliary_indexes][name]
        values, strategies = Array.new(metadata[:columns].length), Array.new(metadata[:columns].length)
        arms.each do |position, operator, source|
          value = scan_constant_value(source)
          return [] if value.nil? || operator == '&&' && value.is_a?(RangeValue) && value.empty
          values[position], strategies[position] = value, operator
        end
        candidates = gist_candidates(stored, name, values, strategies)
        return nil unless candidates
        charge(candidates[:pages] + candidates[:descents])
        ordinals = candidates[:ordinals]
      else
        prepared = arms.map { |arm| gin_query_terms(stored, name, arm) }
        return nil if prepared.any?(&:nil?)
        ordinals = prepared.flat_map { |terms, mode| gather_gin_terms(stored, name, terms, mode) }.uniq { |row| auxiliary_row_identity(row) }
        keys = auxiliary_row_keys(stored)
        ordinals.sort_by! { |ordinal| keys[ordinal] }
      end
      scan_rows(stored, ordinals, lookup: true)
    end

    # The plan inventory uses this same structural matcher. It reads no rows,
    # evaluates no operands, and keeps their expression identities for execution.
    def auxiliary_scan_bound(stored, predicate)
      auxiliary_scan_bounds(stored, predicate).first
    end

    def auxiliary_scan_bounds(stored, predicate)
      return [] unless predicate
      terms = conjunctions(predicate)
      indexes = stored[:native_auxiliary_indexes] || {}
      bounds = []
      indexes.keys.sort_by { |name| [indexes[name][:kind] == 'gist' ? 0 : 1, name.b] }.each do |name|
        metadata = indexes[name]
        next if metadata[:predicate] && !conjunctions(metadata[:predicate]).all? { |part| terms.any? { |term| same_key_expression?(part, term) } }
        arms = metadata[:expressions].each_with_index.filter_map do |key, position|
          match = terms.filter_map { |term| auxiliary_term(term, key, metadata[:kind]) }.first
          [position, *match] if match
        end
        if metadata[:kind] == 'gin'
          arms = arms.first(1)
          if arms.empty?
            terms.each do |term|
              next unless term.kind == :binary && term.value == 'or'
              disjunctions = disjunction_terms(term)
              metadata[:expressions].each_with_index do |key, position|
                matches = disjunctions.map do |branch|
                  match = conjunctions(branch).filter_map { |part| auxiliary_term(part, key, 'gin') }.first
                  match && [position, *match]
                end
                if matches.all?
                  arms = matches
                  break
                end
              end
              break unless arms.empty?
            end
          end
        end
        bounds << { kind: metadata[:kind], name: name, arms: arms } unless arms.empty?
      end
      bounds
    end

    private

    def auxiliary_row_identity(row) = row.is_a?(PagedRow) ? row.storage_key : row

    def auxiliary_row_keys(stored)
      return Hash.new { |_, row| row.storage_key } if stored[:rows].is_a?(PagedRows)
      stored[:native_primary_entries].to_h { |key, ordinal| [ordinal, key] }
    end

    def auxiliary_term(term, key, kind)
      if term.kind == :quantified && kind == 'gin' && term.extra[:op] == '=' && term.extra[:quantifier] != 'all'
        left, right = term.args
        return ['any', left] if same_key_expression?(key, right) && scan_constant?(left)
      end
      return nil unless %i[binary json_operator].include?(term.kind)
      operator = term.value
      allowed = if kind == 'gist'
        Types::RANGES.key?(key.type) ? %w[&& @>] : ['=']
      else
        key.type == 'jsonb' ? %w[@> ? ?| ?&] : %w[@> && =]
      end
      return nil unless allowed.include?(operator)
      left, right = term.args
      left, right = right, left if operator == '=' && same_key_expression?(key, right)
      return nil unless same_key_expression?(key, left) && scan_constant?(right)
      [operator, right]
    end

    def disjunction_terms(expression)
      expression.kind == :binary && expression.value == 'or' ? expression.args.flat_map { |arg| disjunction_terms(arg) } : [expression]
    end

    def scan_constant?(expression)
      expression.kind == :outer || constant_expression?(expression)
    end

    def scan_constant_value(expression)
      cost, lifetime, maximum, lifetime_maximum = @cost, @lifetime_cost, @max_cost, @lifetime_max_cost
      @max_cost, @lifetime_max_cost = 0, 0
      evaluate(expression, [])
    ensure
      @cost, @lifetime_cost, @max_cost, @lifetime_max_cost = cost, lifetime, maximum, lifetime_maximum
    end

    def gin_query_terms(stored, name, arm)
      position, operator, source = arm
      value = scan_constant_value(source)
      return [[], :all] if value.nil?
      metadata = stored[:native_auxiliary_indexes][name]
      type = metadata[:types][position]
      mode = %w[&& ?|].include?(operator) ? :any : :all
      if type == 'jsonb'
        if operator == '@>'
          terms = gin_terms(stored, name, position, value)
        else
          values = operator == '?' ? [value] : value.compact
          prefix = metadata[:columns].length > 1 ? [position].pack('C') : ''.b
          codec = stored[:native_auxiliary_codec]
          terms = values.map { |key| prefix + codec.gin_jsonb_framed(1, key.b) }.uniq.sort
        end
      else
        value = ArrayValue.new([value], element_type: type[0...-2]) if operator == 'any'
        return [[], :all] if operator == '@>' && value.any?(&:nil?)
        element = type.delete_suffix('[]')
        converted = value.filter_map do |item|
          next if item.nil?
          normalized = exact_gin_element(item, element)
          return [[], :all] if normalized.nil? && mode == :all
          normalized
        end
        value = ArrayValue.new(converted, element_type: element)
        terms = gin_terms(stored, name, position, value)
      end
      return nil if terms.empty? && %w[@> = ?&].include?(operator)
      [terms, mode]
    end

    def exact_gin_element(value, type)
      if Types::INTEGER_BITS.key?(type)
        if value.is_a?(Decimal)
          return nil unless value == value.trunc
          value = value.to_i
        end
        return nil unless value.is_a?(Integer)
        bits = Types::INTEGER_BITS[type]
        return nil unless value.between?(-(1 << (bits - 1)), (1 << (bits - 1)) - 1)
      elsif type == 'f32' && value.is_a?(Float)
        converted = [value].pack('f').unpack1('f')
        return nil unless converted == value || value.nan? && converted.nan?
        value = converted
      elsif type == 'decimal' && value.is_a?(Integer)
        value = Decimal.new(value)
      end
      value
    end

    def gather_gin_terms(stored, name, terms, mode)
      return [] if terms.empty?
      metadata = stored[:native_auxiliary_indexes][name]
      postings = metadata[:postings]
      entries = metadata[:entries]
      if mode == :any || terms.length == 1
        return terms.flat_map do |term|
          list = postings[term] || []
          charge(gin_page_count(stored, name, term, prefix_successor(term)) + list.length)
          list
        end.uniq { |row| auxiliary_row_identity(row) }
      end
      height = metadata[:height]
      ordered = terms.map do |term|
        upper = prefix_successor(term)
        span = gin_page_count(stored, name, term, upper)
        peeked = span == height
        if peeked
          charge(span)
          if (paged = metadata[:paged_rows])
            runs = paged.each_entry(lower: term, upper: upper).to_a
          else
            first = entries.bsearch_index { |key, _| key >= term } || entries.length
            last = upper ? entries.bsearch_index { |key, _| key >= upper } || entries.length : entries.length
            runs = entries[first...last]
          end
          [span, runs.length, runs.sum { |key, _| key.bytesize }, term, true]
        else
          [span, Float::INFINITY, Float::INFINITY, term, false]
        end
      end.sort
      span, _, _, term, peeked = ordered.shift
      candidates = postings[term] || []
      charge(span) unless peeked
      charge(candidates.length)
      row_keys = auxiliary_row_keys(stored)
      ordered.each do |_span, _runs, _bytes, next_term, _peeked|
        break if candidates.empty?
        survivors = []
        start = 0
        posting_set = (postings[next_term] || []).to_h { |ordinal| [auxiliary_row_identity(ordinal), true] }
        while start < candidates.length
          lower = next_term + row_keys[candidates[start]]
          finish = start + 1
          while finish < candidates.length
            bound = next_term + row_keys[candidates[finish]]
            break if gin_page_count(stored, name, lower, bound + "\0".b) != height
            finish += 1
          end
          upper_probe = next_term + row_keys[candidates[finish - 1]]
          if (paged = metadata[:paged_rows])
            last_key = paged.each_entry(lower: upper_probe).first&.first
          else
            last_run = entries.bsearch_index { |key, _| key >= upper_probe }
            last_key = last_run && entries[last_run][0]
          end
          upper = if last_key && last_key.start_with?(next_term)
            last_key + "\0".b
          else
            upper_probe + "\0".b
          end
          charge(gin_page_count(stored, name, lower, upper) + finish - start)
          (start...finish).each { |index| survivors << candidates[index] if posting_set[auxiliary_row_identity(candidates[index])] }
          start = finish
        end
        candidates = survivors
      end
      candidates
    end
  end
  Engine.prepend(AuxiliaryScans)
end
