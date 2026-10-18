# frozen_string_literal: true

module JedNative
  # The jsonb_ops query tree follows gin.md §2.2. Extraction only walks the
  # already parsed path; document evaluation remains the residual predicate.
  class JsonPathGin
    def initialize(codec, path, prefix)
      @codec, @strict, @prefix = codec, path.strict, prefix
    end

    def extract(path, operator)
      operator == '@@' ? predicate(path.root, []) : walk(path.root, [], nil)
    end

    def node(mode, parts)
      terms, children = [], []
      parts.compact.each do |part|
        if part[:mode] == mode || part[:children].empty? && part[:terms].length == 1
          terms.concat(part[:terms])
          children.concat(part[:children])
        else
          children << part
        end
      end
      terms = terms.uniq.sort
      return nil if terms.empty? && children.empty?
      return children.first if terms.empty? && children.length == 1
      { mode: mode, terms: terms, children: children }
    end

    def leaf(term) = { mode: :all, terms: [@prefix + term], children: [] }

    def predicate(expression, accessors, negated = false)
      case expression[:kind]
      when :not then predicate(expression[:expr], accessors, !negated)
      when :connective
        mode = (expression[:op] == '&&') != negated ? :all : :any
        parts = [expression[:left], expression[:right]].map { |part| predicate(part, accessors, negated) }
        return nil if mode == :any && parts.any?(&:nil?)
        node(mode, parts)
      when :comparison
        return nil if negated || expression[:op] != '=='
        scalar, path = expression.values_at(:left, :right)
        scalar, path = path, scalar unless scalar[:kind] == :literal
        return nil unless scalar[:kind] == :literal
        walk(path, accessors, scalar)
      when :exists
        walk(expression[:expr], accessors, nil) unless negated
      end
    end

    def walk(expression, enclosing, scalar)
      base = expression[:kind] == :path ? expression[:base] : expression
      return nil unless %i[root current].include?(base[:kind])
      accessors = base[:kind] == :root ? [] : enclosing.dup
      parts = []
      (expression[:steps] || []).each do |step|
        case step[:kind]
        when :filter then parts << predicate(step[:expr], accessors)
        when :member, :members, :descendants, :elements, :subscript then accessors << step
        else return node(:all, parts)
        end
      end
      if scalar
        accessors.reverse_each do |step|
          parts << leaf(@codec.gin_jsonb_framed(1, step[:name])) if step[:kind] == :member
        end
        value = scalar[:value]
        tag = case value
        when nil then [:null]
        when true, false then [:bool, value]
        when String then [:str, value]
        else [:num, value.to_s]
        end
        if value.is_a?(String)
          last = accessors.last&.dig(:kind)
          as_key = @strict && %i[elements subscript].include?(last)
          either = !@strict || last == :descendants
          if either
            parts << node(:any, [leaf(@codec.gin_jsonb_scalar_term(tag, false)), leaf(@codec.gin_jsonb_scalar_term(tag, true))])
          else
            parts << leaf(@codec.gin_jsonb_scalar_term(tag, as_key))
          end
        else
          parts << leaf(@codec.gin_jsonb_scalar_term(tag, false))
        end
      end
      node(:all, parts)
    end
  end

  module GinQueries
    def auxiliary_term(term, key, kind)
      if kind == 'gin' && key.type == 'jsonb'
        if term.kind == :jsonpath_function && term.extra&.dig(:operator)
          left, right = term.args
          return [term.value == 'jsonb_path_match' ? '@@' : '@?', right] if same_key_expression?(key, left) && scan_constant?(right)
        elsif term.kind == :json_operator && term.value == '<@'
          left, right = term.args
          return ['@>', left] if same_key_expression?(key, right) && scan_constant?(left)
        end
      end
      super
    end

    def auxiliary_plan_usable?(stored, bound)
      # A termless query is still a GIN plan; execution falls back to its full
      # scan after extracting the runtime query value (gin.md §6).
      return true if bound[:kind].to_s == 'gin'
      super
    end

    def gin_query_terms(stored, name, arm)
      position, operator, source = arm
      return super unless %w[@? @@].include?(operator)
      value = scan_constant_value(source)
      return [[], :all] if value.nil?
      path = JsonPathValue.parse(value)
      metadata = stored[:native_auxiliary_indexes][name]
      prefix = metadata[:columns].length > 1 ? [position].pack('C') : ''.b
      tree = JsonPathGin.new(stored[:native_auxiliary_codec], path, prefix).extract(path, operator)
      tree && [tree, :tree]
    end

    def gather_gin_terms(stored, name, terms, mode)
      return super unless mode == :tree
      children = terms[:children].dup
      if terms[:mode] == :any
        result = terms[:terms].empty? ? [] : gather_gin_terms(stored, name, terms[:terms], :any)
        children.each { |child| result.concat(gather_gin_terms(stored, name, child, :tree)) }
        result.uniq
      else
        result = if terms[:terms].empty?
                   gather_gin_terms(stored, name, children.shift, :tree)
                 else
                   gather_gin_terms(stored, name, terms[:terms], :all)
                 end
        children.each do |child|
          break if result.empty?
          candidates = gather_gin_terms(stored, name, child, :tree).to_h { |ordinal| [ordinal, true] }
          result.select! { |ordinal| candidates[ordinal] }
        end
        result
      end
    end

    def estimate_predicate(rows, expression)
      if expression&.kind == :jsonpath_function && expression.extra&.dig(:operator)
        estimate = @native_estimate_table && statistics_gin_predicate(expression, @native_estimate_table)
        return estimate_scale(rows, estimate[:rows], @native_estimate_table[:rows].length) if estimate
        return plan_constant(expression.args[1]).nil? ? 0 : (rows + 99) / 100
      end
      super
    end

    def statistics_gin_predicate(expression, stored)
      bound = auxiliary_scan_bounds(stored, expression).find { |candidate| candidate[:kind] == 'gin' }
      return nil if bound && bound[:arms].any? { |_, operator, _| operator == '=' }
      bound && statistics_gin_bound(stored, bound)
    end

    def statistics_gin_bound(stored, bound)
      metadata = stored[:native_auxiliary_indexes][bound[:name]]
      arms = bound[:arms].map do |position, operator, source|
        distribution = statistics_distribution(stored, metadata[:columns][position])
        return nil unless distribution && distribution[:fact][:terms]
        value = plan_constant(source)
        return nil if value == :unknown
        replacement = Expression.new(kind: :literal, type: source.type, value: value)
        prepared = gin_query_terms(stored, bound[:name], [position, operator, replacement])
        return nil unless prepared
        terms, mode = prepared
        tree = mode == :tree ? terms : { mode: mode, terms: terms, children: [] }
        estimate_gin_tree(tree, distribution, metadata[:columns].length > 1)
      end
      total = metadata[:columns].sum do |ordinal|
        fact = stored[:storage_statistics].find { |entry| entry[:column] == ordinal }
        return nil unless fact && fact[:terms] && fact[:analyzed_rows].positive?
        estimate_scale(stored[:rows].length, fact[:terms][:occurrences], fact[:analyzed_rows])
      end
      { rows: [stored[:rows].length, arms.sum { |arm| arm[:rows] }].min,
        entries: arms.sum { |arm| arm[:entries] }, total_entries: total }
    end

    def estimate_gin_tree(tree, distribution, tagged)
      fact = distribution[:fact]
      counts = tree[:terms].map do |term|
        term = term.byteslice(1..) if tagged
        frequency = fact[:terms][:entries].find { |key, _| key == term }&.last
        count = if frequency
          [estimate_scale(distribution[:rows], frequency, fact[:analyzed_rows]), distribution[:nonnull]].min
        elsif fact[:terms][:untracked_rows].zero? && !fact[:stale]
          0
        else
          [1, [estimate_scale(distribution[:rows], (fact[:terms][:untracked_rows] + 1) / 2, fact[:analyzed_rows]), distribution[:nonnull]].min].max
        end
        [count, term]
      end.sort
      children = tree[:children].map { |child| estimate_gin_tree(child, distribution, tagged) }
      if tree[:mode] == :any
        entries = counts.sum(&:first) + children.sum { |child| child[:entries] }
        rows = [distribution[:nonnull], counts.sum(&:first) + children.sum { |child| child[:rows] }].min
      else
        first = counts.empty? ? children.shift : { rows: counts.first[0], entries: counts.first[0] }
        rows, entries = first ? first.values_at(:rows, :entries) : [0, 0]
        counts.drop(1).each do |count, _|
          entries += rows
          rows = estimate_scale(rows, count, distribution[:rows])
        end
        children.each do |child|
          entries += child[:entries]
          rows = estimate_scale(rows, child[:rows], distribution[:rows])
        end
      end
      { rows: rows, entries: entries }
    end
  end
  Engine.prepend(GinQueries)
end
