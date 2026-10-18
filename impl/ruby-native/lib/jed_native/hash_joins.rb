# frozen_string_literal: true

module JedNative
  # The hash table is lookup-only: build entries retain input order and probe
  # rows determine output order. Canonical key bytes also define work charges.
  module HashJoins
    def hash_join_keys(predicate, left_width)
      return unless predicate
      terms = conjunctions(predicate)
      return unless terms.all? do |term|
        term.kind == :binary && %w[= != <>].include?(term.value) &&
          term.args.all? { |arg| %i[column literal].include?(arg.kind) }
      end
      keys = terms.filter_map do |term|
        next unless term.value == '=' && term.args.all? { |arg| arg.kind == :column }
        left, right = term.args
        left, right = right, left if left.value >= left_width
        next unless left.value < left_width && right.value >= left_width
        next unless left.type == right.type && hash_join_keyable?(left.type)
        [left.value, right.value - left_width, left.type]
      end
      keys unless keys.empty?
    end

    def hash_join_keyable?(type)
      return false if @types.key?(type) || %w[record json jsonb jsonpath unknown null].include?(type)
      if type.end_with?('[]')
        element = type.delete_suffix('[]')
        return !element.end_with?('[]') && !Types::RANGES.key?(element) && hash_join_keyable?(element)
      end
      element = Types::RANGES[type]
      !element || hash_join_keyable?(element)
    end

    def hash_join_table(left, right, predicate, kind, gate: predicate, force: false)
      return unless %i[inner left left_outer].include?(kind)
      keys = hash_join_keys(gate, left.columns.length)
      return unless keys
      unless force || %i[left left_outer].include?(kind)
        # Ordinary scans are common to these two candidates. Compare their
        # remaining work using the shared row-count fallback and key widths.
        pairs = left.rows.length * right.rows.length
        on_work = hash_join_operator_count(predicate)
        key_bytes = keys.sum { |_, _, type| FileFormat::STORAGE_WIDTH.fetch(type, 1) }
        matches = keys.reduce(pairs) { |rows, _| (rows + 199) / 200 }
        hash_cost = (left.rows.length + right.rows.length) * key_bytes +
          matches * (key_bytes + 4 * keys.length + on_work)
        return if hash_cost > pairs * on_work
      end
      codec = statistics_codec
      buckets = {}
      owner = query_memory_owner if @query_memory
      right.rows.each_with_index do |row, index|
        bytes = hash_join_row_key(row, keys, 1, codec)
        next unless bytes
        if owner
          size = keys.sum(MemorySchedule::ENTRY) { |_, ordinal, type| LogicalMemory.value_bytes(row[ordinal], type) }
          query_memory_add(owner, size)
        end
        (buckets[statistics_hash(bytes)] ||= []) << [bytes, index]
      end
      [keys, codec, buckets, owner]
    end

    def hash_join_release(table)
      query_memory_release(table[3]) if @query_memory
    end

    def hash_join_operator_count(expression)
      own = %i[column literal param outer].include?(expression.kind) ? 0 : 1
      own + (expression.args || []).sum { |arg| hash_join_operator_count(arg) }
    end

    def hash_join_row_key(row, keys, side, codec)
      parts = []
      present = true
      keys.each do |key|
        value, type = row[key[side]], key[2]
        if value.nil?
          charge
          present = false
        else
          body = codec.key_body(type, statistics_value(type, value))
          charge([body.bytesize, 1].max)
          parts << [body.bytesize].pack('N') << body
        end
      end
      parts.join.b if present
    end

    def hash_join_candidates(table, row)
      keys, codec, buckets = table
      bytes = hash_join_row_key(row, keys, 0, codec)
      return [] unless bytes
      (buckets[statistics_hash(bytes)] || []).filter_map do |candidate, index|
        charge([[bytes.bytesize, candidate.bytesize].min, 1].max)
        index if bytes == candidate
      end
    end
  end

  Engine.prepend(HashJoins)
end
