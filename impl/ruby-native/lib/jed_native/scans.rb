# frozen_string_literal: true

module JedNative
  # A secondary bound selects immutable row ordinals. The original predicate
  # remains the residual filter, including expression keys and partial indexes.
  module IndexScans
    def select(statement)
      previous = @native_scan_columns
      @native_scan_columns = scan_column_inventory(statement)
      super
    ensure
      @native_scan_columns = previous
    end

    def update(statement)
      previous = @native_scan_columns
      @native_scan_columns = scan_column_inventory(statement, mutation: true)
      super
    ensure
      @native_scan_columns = previous
    end

    def delete(statement)
      previous = @native_scan_columns
      @native_scan_columns = scan_column_inventory(statement, mutation: true)
      super
    ensure
      @native_scan_columns = previous
    end

    def primary_scan(stored, predicate)
      result = primary_key_scan(stored, predicate)
      return result unless result.nil?
      secondary_scan(stored, predicate)
    end

    def full_scan_pages(stored)
      stored[:native_primary_metadata] ? primary_page_count(stored) : (stored[:rows].empty? ? 0 : 1)
    end

    def charge_scan_payload(stored, ordinals = nil, columns = nil)
      return unless stored[:native_primary_metadata]
      columns ||= @native_scan_columns&.[](stored[:name])
      charge(primary_scan_units(stored, ordinals, columns).sum)
    end

    def scan_rows(stored, ordinals, lookup: false)
      reverse = @native_reverse_scan
      iterator = Enumerator.new do |output|
        sequence = reverse ? ordinals.reverse_each : ordinals.each
        sequence.each do |ordinal|
          if lookup
            charge(stored.dig(:native_primary_metadata, :height) || 1)
            charge_scan_payload(stored, [ordinal])
          end
          output << stored[:rows][ordinal]
        end
      end
      @native_stream_scan ? iterator : iterator.to_a
    end

    private

    def scan_column_inventory(statement, mutation: false)
      sources = if mutation
        [{ kind: :table, name: statement[:table], alias: statement[:alias] }]
      else
        (statement[:from] || []) + (statement[:joins] || []).map { |item| item[:relation] }
      end
      result = {}
      sources = [statement[:native_scan_source]] if statement[:native_scan_source]
      sources.each do |source|
        next unless source[:kind] == :table && (stored = @tables[source[:name]])
        labels = [source[:alias] || source[:name]]
        labels.concat(%w[old new]) if mutation
        ordinals = []
        visit = lambda do |node, nested = false|
          case node
          when Array then node.each { |child| visit.call(child, nested) }
          when Hash
            if node[:kind] == :column && (node[:table] ? labels.include?(node[:table]) : !nested)
              ordinal = stored[:columns].index { |column| column[:name] == node[:name] }
              ordinals << ordinal if ordinal
            elsif node[:kind] == :star && (node[:table] ? labels.include?(node[:table]) : !nested)
              ordinals.concat(stored[:columns].each_index.to_a)
            elsif node[:kind] == :call && node[:name] == 'count' && node[:args]&.first&.dig(:kind) == :star
              visit.call(node[:filter], nested)
            else
              node.each { |key, child| visit.call(child, nested || key == :query) }
            end
          end
        end
        visit.call(statement)
        unless mutation || (statement[:group_by] || []).empty?
          analyzer = ParameterBindings.new(self, statement)
          scope = query_input_columns(statement, @ctes.transform_values(&:columns), [], analyzer)
          visit.call(resolve_group_nodes(statement, scope))
        end
        stored[:constraints].each { |constraint| visit.call(constraint[:expr]) if constraint[:kind] == :check } if mutation
        result[stored[:name]] = ordinals.uniq.sort
      end
      result
    end

    def primary_key_scan(stored, predicate)
      metadata = stored[:native_primary_metadata]
      return nil unless metadata && !metadata[:columns].empty? && predicate
      terms = conjunctions(predicate)
      values, range = [], nil
      metadata[:columns].each_with_index do |column, i|
        key = Expression.new(kind: :column, type: metadata[:types][i], value: column)
        comparisons = key_comparisons(key, terms)
        break if comparisons.empty?
        equality = comparisons.find { |operator, _| operator == '=' || operator == 'is null' }
        if equality
          operator, source = equality
          return [] if operator == 'is null'
          value = evaluate(source, [])
          return [] if value.nil?
          value = normalize_scan_bound(key.type, value)
          return [] if value == :unmatchable
          if (bits = Types::INTEGER_BITS[key.type]) && !value.between?(-(1 << (bits - 1)), (1 << (bits - 1)) - 1)
            return []
          end
          values << value
        else
          range = [i, comparisons]
          break
        end
      end
      return nil if values.empty? && !range
      prefix = primary_prefix_key(stored, values)
      lower, upper = prefix.empty? ? [nil, nil] : [prefix, prefix_successor(prefix)]
      if range
        i, comparisons = range
        comparisons.each do |operator, source|
          value = evaluate(source, [])
          return [] if value.nil?
          value = normalize_scan_bound(metadata[:types][i], value)
          next if value == :unmatchable
          if (bits = Types::INTEGER_BITS[metadata[:types][i]])
            minimum, maximum = -(1 << (bits - 1)), (1 << (bits - 1)) - 1
            if value < minimum
              return [] if %w[< <=].include?(operator)
              next
            elsif value > maximum
              return [] if %w[> >=].include?(operator)
              next
            end
          end
          key = primary_prefix_key(stored, values + [value])
          if %w[> >=].include?(operator)
            bound = operator == '>' ? prefix_successor(key) : key
            return [] unless bound
            lower = bound if lower.nil? || bound > lower
          else
            bound = operator == '<=' ? prefix_successor(key) : key
            upper = bound if bound && (upper.nil? || bound < upper)
          end
        end
      end
      return [] if lower && upper && lower >= upper
      if @native_stream_scan && @native_ordered_scan
        return storage_stream_rows(stored, lower: lower, upper: upper, reverse: @native_reverse_scan)
      end
      entries = stored[:native_primary_entries]
      if stored[:rows].is_a?(PagedRows)
        ordinals = paged_scan_ordinals(stored, lower: lower, upper: upper)
      else
        first = lower ? entries.bsearch_index { |key, _| key >= lower } || entries.length : 0
        last = upper ? entries.bsearch_index { |key, _| key >= upper } || entries.length : entries.length
        ordinals = entries[first...last].map(&:last)
      end
      charge(primary_page_count(stored, lower, upper))
      charge_scan_payload(stored, ordinals)
      scan_rows(stored, ordinals)
    end

    def secondary_scan(stored, predicate, selected_name: nil, allow_full: false)
      indexes = stored[:native_secondary_indexes]
      return nil unless indexes && predicate
      terms = conjunctions(predicate)
      choices = []
      indexes.keys.sort_by(&:b).each do |name|
        next if selected_name && name != selected_name
        details = stored[:native_secondary_metadata][name]
        next unless partial_predicate_implied?(details[:predicate], terms)
        equalities = []
        range = nil
        empty = false
        details[:expressions].each_with_index do |expression, ordinal|
          comparisons = key_comparisons(expression, terms)
          break if comparisons.empty?
          equality = comparisons.find { |operator, _| operator == '=' || operator == 'is null' }
          if equality
            operator, value_expression = equality
            value = operator == 'is null' ? nil : evaluate(value_expression, [])
            if value.nil? && operator != 'is null'
              empty = true
              break
            end
            value = normalize_scan_bound(expression.type, value) unless value.nil?
            return [] if value == :unmatchable
            equalities << value
          else
            range = [ordinal, comparisons]
            break
          end
        end
        return [] if empty
        next if equalities.empty? && !range && !allow_full
        prefix = secondary_prefix_key(stored, name, equalities)
        lower = prefix.empty? ? nil : prefix
        upper = prefix.empty? ? nil : prefix_successor(prefix)
        if range
          ordinal, comparisons = range
          encodable = comparisons.reject { |_, source| details[:types][ordinal] == 'decimal' && source.extra&.dig(:native_integer_bound) }
          unless encodable.empty?
            present, = stored[:native_secondary_codec].ordered_slot_tags(*details[:orders][ordinal])
            lower = prefix + [present].pack('C')
            upper = prefix_successor(lower)
          end
          comparisons.each do |operator, value_expression|
            # The shared bound encoder widens an integer-literal endpoint
            # whose comparison adapted to decimal; the residual checks it.
            next if details[:types][ordinal] == 'decimal' && value_expression.extra&.dig(:native_integer_bound)
            value = evaluate(value_expression, [])
            return [] if value.nil?
            value = normalize_scan_bound(details[:types][ordinal], value)
            next if value == :unmatchable
            key = secondary_prefix_key(stored, name, equalities + [value])
            if details[:orders][ordinal][0]
              operator = { '<' => '>', '<=' => '>=', '>' => '<', '>=' => '<=' }.fetch(operator)
            end
            if operator == '>' || operator == '>='
              bound = operator == '>' ? prefix_successor(key) : key
              return [] unless bound
              lower = bound if lower.nil? || bound > lower
            else
              bound = operator == '<=' ? prefix_successor(key) : key
              upper = bound if bound && (upper.nil? || bound < upper)
            end
          end
        end
        return [] if lower && upper && lower >= upper
        entries = indexes[name]
        if stored[:rows].is_a?(PagedRows)
          selected = paged_scan_ordinals(stored, name, lower: lower, upper: upper)
          choices << [selected.length, name.b, name, nil, nil, lower, upper, selected]
        else
          first = lower ? entries.bsearch_index { |key, _| key >= lower } || entries.length : 0
          last = upper ? entries.bsearch_index { |key, _| key >= upper } || entries.length : entries.length
          choices << [last - first, name.b, name, first, last, lower, upper, nil]
        end
      end
      choice = choices.min
      return nil unless choice
      _, _, name, first, last, lower, upper, selected = choice
      entries = indexes[name]
      pages = if respond_to?(:secondary_page_count)
        secondary_page_count(stored, name, lower, upper)
      else
        entries.empty? ? 0 : 1
      end
      ordinals = selected || entries[first...last].map(&:last)
      charge(pages)
      scan_rows(stored, ordinals, lookup: true)
    end

    def conjunctions(expression)
      expression.kind == :binary && expression.value == 'and' ? expression.args.flat_map { |arg| conjunctions(arg) } : [expression]
    end

    def key_comparisons(key, terms)
      terms.filter_map do |term|
        if term.kind == :unary && term.value == 'is null' && same_key_expression?(key, term.args[0])
          next ['is null', nil]
        end
        next unless term.kind == :binary && %w[= < <= > >=].include?(term.value)
        left, right = term.args
        next if [left, right].any? { |argument| argument.extra&.dig(:subquery_origin) }
        operator = term.value
        if same_key_expression?(key, right)
          left, right = right, left
          operator = { '=' => '=', '<' => '>', '<=' => '>=', '>' => '<', '>=' => '<=' }.fetch(operator)
        end
        next unless same_key_expression?(key, left) && %i[literal param outer].include?(right.kind)
        compatible = key.type == right.type || %w[null unknown].include?(right.type) || Types::INTEGER_BITS.key?(key.type) && Types::INTEGER_BITS.key?(right.type)
        compatible ||= right.kind == :outer && (Types::INTEGER_BITS.key?(key.type) && right.type == 'decimal' || key.type == 'decimal' && Types::INTEGER_BITS.key?(right.type))
        next unless compatible
        [operator, right]
      end
    end

    def normalize_scan_bound(type, value)
      if Types::INTEGER_BITS.key?(type) && value.is_a?(Decimal)
        return :unmatchable unless value == value.trunc
        integer = value.to_i
        bits = Types::INTEGER_BITS[type]
        return integer.between?(-(1 << (bits - 1)), (1 << (bits - 1)) - 1) ? integer : :unmatchable
      end
      type == 'decimal' && value.is_a?(Integer) ? Decimal.new(value) : value
    end

    def same_key_expression?(left, right)
      return false unless left.is_a?(Expression) && right.is_a?(Expression)
      if left.kind == :column && right.kind == :column && left.value == right.value &&
         Types::INTEGER_BITS.key?(left.type) && Types::INTEGER_BITS.key?(right.type) &&
         (left.extra&.dig(:native_body_type) || right.extra&.dig(:native_body_type))
        return true
      end
      return false unless [left.kind, left.type, left.value] == [right.kind, right.type, right.value]
      return false if left.kind == :cast && left.extra&.dig(:type_mod) != right.extra&.dig(:type_mod)
      a, b = left.args || [], right.args || []
      return false unless a.length == b.length
      a.zip(b).all? { |x, y| x.is_a?(Expression) ? same_key_expression?(x, y) : x == y }
    end

    def prefix_successor(prefix)
      bytes = prefix.bytes
      bytes.pop while bytes.last == 255
      return nil if bytes.empty?
      bytes[-1] += 1
      bytes.pack('C*')
    end
  end
  Engine.prepend(IndexScans)
end
