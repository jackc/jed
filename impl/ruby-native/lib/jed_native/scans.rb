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

    private

    def scan_column_inventory(statement, mutation: false)
      sources = if mutation
        [{ kind: :table, name: statement[:table], alias: statement[:alias] }]
      else
        (statement[:from] || []) + (statement[:joins] || []).map { |item| item[:relation] }
      end
      result = {}
      sources.each do |source|
        next unless source[:kind] == :table && (stored = @tables[source[:name]])
        labels = [source[:alias] || source[:name]]
        labels.concat(%w[old new]) if mutation
        ordinals = []
        visit = lambda do |node|
          case node
          when Array then node.each { |child| visit.call(child) }
          when Hash
            if node[:kind] == :column && (!node[:table] || labels.include?(node[:table]))
              ordinal = stored[:columns].index { |column| column[:name] == node[:name] }
              ordinals << ordinal if ordinal
            elsif node[:kind] == :star && (!node[:table] || labels.include?(node[:table]))
              ordinals.concat(stored[:columns].each_index.to_a)
            elsif node[:kind] == :call && node[:name] == 'count' && node[:args]&.first&.dig(:kind) == :star
              visit.call(node[:filter])
            else
              node.each_value { |child| visit.call(child) }
            end
          end
        end
        visit.call(statement)
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
      entries = stored[:native_primary_entries]
      first = lower ? entries.bsearch_index { |key, _| key >= lower } || entries.length : 0
      last = upper ? entries.bsearch_index { |key, _| key >= upper } || entries.length : entries.length
      ordinals = entries[first...last].map(&:last)
      charge(primary_page_count(stored, lower, upper))
      charge_scan_payload(stored, ordinals)
      ordinals.map { |ordinal| stored[:rows][ordinal] }
    end

    def secondary_scan(stored, predicate)
      indexes = stored[:native_secondary_indexes]
      return nil unless indexes && predicate
      terms = conjunctions(predicate)
      choices = []
      indexes.keys.sort_by(&:b).each do |name|
        details = stored[:native_secondary_metadata][name]
        next if details[:predicate] && !conjunctions(details[:predicate]).all? { |part| terms.any? { |term| same_key_expression?(part, term) } }
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
            equalities << value
          else
            range = [ordinal, comparisons]
            break
          end
        end
        return [] if empty
        next if equalities.empty? && !range
        prefix = secondary_prefix_key(stored, name, equalities)
        lower = prefix.empty? ? nil : prefix
        upper = prefix.empty? ? nil : prefix_successor(prefix)
        if range
          ordinal, comparisons = range
          comparisons.each do |operator, value_expression|
            value = evaluate(value_expression, [])
            return [] if value.nil?
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
        first = lower ? entries.bsearch_index { |key, _| key >= lower } || entries.length : 0
        last = upper ? entries.bsearch_index { |key, _| key >= upper } || entries.length : entries.length
        choices << [last - first, name.b, name, first, last, lower, upper]
      end
      choice = choices.min
      return nil unless choice
      _, _, name, first, last, lower, upper = choice
      entries = indexes[name]
      pages = if respond_to?(:secondary_page_count)
        secondary_page_count(stored, name, lower, upper)
      else
        entries.empty? ? 0 : 1
      end
      ordinals = entries[first...last].map(&:last)
      charge(pages + (last - first) * (stored.dig(:native_primary_metadata, :height) || 1))
      charge_scan_payload(stored, ordinals)
      ordinals.map { |ordinal| stored[:rows][ordinal] }
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
        operator = term.value
        if same_key_expression?(key, right)
          left, right = right, left
          operator = { '=' => '=', '<' => '>', '<=' => '>=', '>' => '<', '>=' => '<=' }.fetch(operator)
        end
        next unless same_key_expression?(key, left) && %i[literal param outer].include?(right.kind)
        compatible = key.type == right.type || %w[null unknown].include?(right.type) || Types::INTEGER_BITS.key?(key.type) && Types::INTEGER_BITS.key?(right.type)
        next unless compatible
        [operator, right]
      end
    end

    def same_key_expression?(left, right)
      return false unless left.is_a?(Expression) && right.is_a?(Expression)
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
