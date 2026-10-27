# frozen_string_literal: true

module JedNative
  # Per-row key metering (spec/design/cost.md §3 "sort_key / hash_key"). A sort,
  # a grouping or dedup table, and a window's partitioning charge each row's key
  # values once, by logical payload, where the row enters the operator. The
  # comparisons and probes inside them stay unmetered. A charge that is zero
  # adds no guard, so fixed-width and NULL keys move no abort point.
  module KeyMetering
    # One sort-key value: collate per code point for a collated text or text[]
    # key, otherwise sort_key per payload byte.
    def charge_sort_key(value)
      units = if (value.is_a?(String) || value.is_a?(Array)) && (label = CollationRules.label(value)) && label.table && !label.conflict
        collation_points(value)
      else
        LogicalMemory.payload(value)
      end
      charge(units) if units.positive?
    end

    def charge_sort_keys(values)
      values.each { |value| charge_sort_key(value) }
    end

    # The key values of a row a grouping, dedup, or partition table receives.
    def charge_hash_keys(values)
      values.each do |value|
        units = LogicalMemory.payload(value)
        charge(units) if units.positive?
      end
    end

    def charge_hash_key(value)
      units = LogicalMemory.payload(value)
      charge(units) if units.positive?
    end
  end
  Engine.include(KeyMetering)
end
