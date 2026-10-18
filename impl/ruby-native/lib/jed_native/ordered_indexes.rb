# frozen_string_literal: true

module JedNative
  # An ordered index emits equal sort keys in primary-key order. Only an exact
  # match of every key's direction and NULL placement can replace stable sort.
  module OrderedIndexes
    def scan_access_plan(stored, predicate, statement = nil)
      selected = super
      return selected unless statement && statement[:limit] && !(statement[:order_by] || []).empty? && streaming_select_shape?(statement)
      return selected unless stored[:collation_skew].to_a.empty?
      candidates = [selected]
      terms = predicate ? conjunctions(predicate) : []
      existing = scan_access_candidates(stored, predicate).filter_map { |candidate| candidate[:name] if %i[secondary secondary_interval].include?(candidate[:kind]) }
      (stored[:native_secondary_metadata] || {}).sort_by { |name, _| name.b }.each do |name, metadata|
        next if existing.include?(name) || !partial_predicate_implied?(metadata[:predicate], terms)
        candidate = { kind: :secondary, name: name, terms: [], parts: [], partial: metadata[:predicate], native_order_only: true }
        next unless scan_candidate_order(statement, stored, candidate) == :forward
        estimate_scan_candidate(stored, predicate, candidate, statement)
        candidates << candidate
      end
      candidates.min_by { |candidate| [candidate[:pipeline_cost], AccessPlanning::ACCESS_RANK.fetch(candidate[:kind]), candidate[:name].to_s.b] }
    end

    def scan_candidate_order(statement, stored, candidate)
      return super unless %i[secondary secondary_interval].include?(candidate[:kind])
      return :blocking unless statement && streaming_select_shape?(statement)
      orders = statement[:order_by] || []
      return :none if orders.empty?
      metadata = stored[:native_secondary_metadata][candidate[:name]]
      keys = metadata[:expressions]
      return :blocking unless orders.length == keys.length
      primary = stored[:native_primary_metadata]
      return :blocking unless primary && primary[:types].all? { |type| FileFormat::STORAGE_WIDTH.key?(type) }
      source = statement[:from].first
      scope = table_columns(stored, source[:alias] || source[:name])
      projections = expand_stars(statement[:columns], scope)
      valid = orders.each_with_index.all? do |order, index|
        descending = order[:direction] == :desc
        nulls_first = order[:nulls] ? order[:nulls] == :first : descending
        next false unless metadata[:orders][index] == [descending, nulls_first]
        node = order[:expr]
        if node[:kind] == :literal && node[:value].is_a?(Integer)
          node = projections[node[:value] - 1]&.dig(:expr)
        elsif node[:kind] == :column && !node[:table]
          projection = projections.find { |entry| (entry[:alias] || expression_name(entry[:expr])) == node[:name] }
          node = projection[:expr] if projection
        end
        node && metadata_expression?(node) && same_key_expression?(compile(node, scope), keys[index])
      end
      valid ? :forward : :blocking
    end

    def plan_scan(stored, predicate, statement, access = nil)
      node = super
      node.detail = node.detail.sub(/\AIndex scan: using [^;]+/, 'Full scan') if node.access[:native_order_only]
      node
    end

    def execute_scan_access(stored, predicate, access)
      return super unless access[:native_order_only]
      # An order-only walk prepays the complete index-tree block; LIMIT bounds
      # table lookups and projections, as specified by cost.md §3.
      charge(stored[:native_secondary_metadata][access[:name]][:node_count])
      iterator = Enumerator.new do |output|
        storage_scan_entries(stored, access[:name]).each do |_, ordinal|
          charge(stored[:native_primary_metadata][:height])
          charge_scan_payload(stored, [ordinal])
          output << storage_row(stored, ordinal)
        end
      end
      @native_stream_scan ? iterator : iterator.to_a
    end
  end
  Engine.prepend(OrderedIndexes)
end
