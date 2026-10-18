# frozen_string_literal: true

require_relative 'file_format'

module JedNative
  # Deterministic maintenance facts. Collection retains a bounded sample and KMV
  # set; the experimental engine's row store still owns the full population.
  module Maintenance
    SAMPLE_ROWS = 30_000
    KMV_HASHES = 4096
    STATISTIC_ENTRIES = 100
    HISTOGRAM_BOUNDS = 101
    RETAINED_BYTES = 128
    MAX_I64 = (1 << 63) - 1
    MASK64 = (1 << 64) - 1

    # A fixed-capacity max heap retains the smallest priorities. Entries compare
    # only their first two fields, so SQL values never become tie breakers.
    class RetainedHeap
      attr_reader :entries

      def initialize(limit)
        @limit, @entries = limit, []
      end

      def retain(entry)
        if @entries.length < @limit
          @entries << entry
          position = @entries.length - 1
          while position.positive?
            parent = (position - 1) / 2
            break unless greater?(@entries[position], @entries[parent])
            @entries[position], @entries[parent] = @entries[parent], @entries[position]
            position = parent
          end
          return nil
        end
        return entry unless greater?(@entries.first, entry)
        removed = @entries.first
        @entries[0] = entry
        position = 0
        loop do
          left = 2 * position + 1
          break if left >= @entries.length
          right = left + 1
          child = right < @entries.length && greater?(@entries[right], @entries[left]) ? right : left
          break unless greater?(@entries[child], @entries[position])
          @entries[position], @entries[child] = @entries[child], @entries[position]
          position = child
        end
        removed
      end

      private

      def greater?(left, right) = left[0] > right[0] || left[0] == right[0] && left[1] > right[1]
    end

    def dispatch(statement)
      case statement[:kind]
      when :analyze then analyze_table(statement)
      when :reindex then reindex(statement)
      else super
      end
    end

    def insert(statement)
      before = statistics_row_snapshots
      result = super
      mark_changed_statistics(before) if result.rows_affected.positive?
      result
    end

    def update(statement)
      before = statistics_row_snapshots
      result = super
      mark_changed_statistics(before) if result.rows_affected.positive?
      result
    end

    def delete(statement)
      before = statistics_row_snapshots
      result = super
      mark_changed_statistics(before) if result.rows_affected.positive?
      result
    end

    def alter_table(statement)
      result = super
      name = statement[:action] == :rename ? statement[:new_name] : statement[:name]
      stored = @tables[name]
      if stored && stored[:storage_statistics]
        actions = statement[:actions] || []
        facts = actions.any? { |action| %i[drop_column set_type].include?(action[:kind]) } ? [] : stored[:storage_statistics]
        @tables[name] = stored.merge(storage_statistics: facts, storage_rows: stored[:rows])
      end
      result
    end

    private

    def statistics_value(type, value)
      Types.storage_value(type, value, @types)
    end

    def statistics_codec
      codec = FileFormat.new(collation_key: ->(name, value) { resolve_collation(name).sort_key(value) })
      codec.instance_variable_set(:@ctypes, @types)
      codec
    end

    def statistics_storage_rows(table, codec)
      names = table[:constraints].find { |constraint| constraint[:kind] == :primary_key }&.dig(:columns) || []
      primary = names.map { |name| table[:columns].index { |column| column[:name] == name } }
      table[:rows].each_with_index.map do |row, index|
        key = if primary.empty?
                codec.encode_int(8, table[:rowids]&.at(index) || index)
              else
                primary.map do |ordinal|
                  column = table[:columns][ordinal]
                  codec.key_body(column[:type], statistics_value(column[:type], row[ordinal]), column[:collation] == 'C' ? nil : column[:collation])
                end.join.b
              end
        [key, row]
      end.sort_by(&:first)
    end

    def statistics_hash(bytes)
      hash = 0xcbf29ce484222325
      bytes.each_byte { |byte| hash = ((hash ^ byte) * 0x100000001b3) & MASK64 }
      hash
    end

    def analyze_table(statement)
      name = statement[:table]
      raise Error.new('42809', 'cannot analyze system relation') if introspection_relation?(name)
      introspection_scope(statement[:database])
      return Result.new if @views.key?(name)
      stored = table(name)
      names = statement[:columns] || stored[:columns].map { |column| column[:name] }
      raise Error.new('42701', 'column appears more than once') unless names.uniq.length == names.length
      columns = names.map do |column|
        stored[:columns].index { |item| item[:name] == column } || raise(Error.new('42703', "unknown column #{column}"))
      end
      codec = statistics_codec
      rows = statistics_storage_rows(stored, codec)
      facts = (stored[:storage_statistics] || []).map do |fact|
        fact.merge(stale: fact[:stale] || stored[:storage_rows] && stored[:storage_rows] != stored[:rows])
      end.to_h { |fact| [fact[:column], fact] }
      columns.each { |column| facts[column] = collect_statistics(stored, column, rows, codec) }
      @tables[name] = stored.merge(storage_statistics: facts.values.sort_by { |fact| fact[:column] }, storage_rows: stored[:rows])
      @dirty = true
      Result.new
    end

    def collect_statistics(stored, ordinal, rows, codec)
      column = stored[:columns][ordinal]
      type = column[:type]
      eligible = !@types.key?(type) && !type.end_with?('[]') && !%w[json jsonb jsonpath record].include?(type)
      term_eligible = codec.term_eligible?(type)
      sample, hashes, seen = RetainedHeap.new(SAMPLE_ROWS), RetainedHeap.new(KMV_HASHES), {}
      overflow = false
      null_count, width_sum, occurrences = 0, 0, 0
      terms, pruned, oversized_terms = {}, false, false
      charge(primary_page_count(stored))
      positions = stored[:rows].each_with_index.to_h { |row, index| [row.object_id, index] }
      rows.each_with_index do |(storage_key, row), position|
        charge(1 + primary_scan_units(stored, [positions.fetch(row.object_id)], [ordinal]).sum)
        value = statistics_value(type, row[ordinal])
        priority = statistics_hash(storage_key)
        if value.nil?
          charge
          null_count += 1
          sample.retain([priority, position, false, false, nil, nil])
          next
        end
        body = codec.encode_value(type, value)
        key = eligible ? codec.key_body(type, value, column[:collation] == 'C' ? nil : column[:collation]) : nil
        width = key ? key.bytesize : body.bytesize - 1
        charge([width, 1].max)
        width_sum = [MAX_I64, width_sum + width].min
        if eligible
          hash = statistics_hash(key)
          unless seen[hash]
            overflow ||= seen.length == KMV_HASHES
            removed = hashes.retain([hash, 0])
            seen.delete(removed[0]) if removed
            seen[hash] = true unless removed && removed[0] == hash
          end
        end
        if term_eligible
          codec.gin_value_terms(type, value).each do |term|
            charge([term.bytesize, 1].max)
            occurrences = [MAX_I64, occurrences + 1].min
            bucket = (occurrences + FileFormat::TERM_BUCKET_WIDTH - 1) / FileFormat::TERM_BUCKET_WIDTH
            if term.bytesize > RETAINED_BYTES
              oversized_terms = true
            elsif terms.key?(term)
              terms[term][0] += 1
            else
              terms[term] = [1, bucket - 1]
            end
            if (occurrences % FileFormat::TERM_BUCKET_WIDTH).zero?
              before = terms.length
              terms.reject! { |_, (count, delta)| count + delta <= bucket }
              pruned ||= terms.length != before
            end
          end
        end
        oversized = body.bytesize - 1 > RETAINED_BYTES || key && key.bytesize > RETAINED_BYTES
        sample.retain([priority, position, true, !!oversized, eligible && !oversized ? key : nil, eligible && !oversized ? value : nil])
      end
      distinct = if eligible
                   if overflow
                     denominator = hashes.entries.first[0] + 1
                     estimate = (((KMV_HASHES - 1) << 64) + denominator - 1) / denominator
                     [[estimate, KMV_HASHES + 1].max, rows.length - null_count].min
                   else
                     seen.length
                   end
                 end
      sampled = sample.entries
      nonnull = sampled.count { |entry| entry[2] }
      groups = {}
      sampled.select { |entry| entry[4] }.sort_by { |entry| [entry[4], entry[1]] }.each do |entry|
        if groups.key?(entry[4])
          groups[entry[4]][1] += 1
        else
          groups[entry[4]] = [entry[5], 1]
        end
      end
      oversized = sampled.any? { |entry| entry[3] }
      all_groups = rows.length <= SAMPLE_ROWS && !oversized && groups.length <= STATISTIC_ENTRIES
      selected = groups.select { |_, (_, frequency)| all_groups || frequency >= 2 && frequency * distinct > nonnull }
                       .sort_by { |key, (_, frequency)| [-frequency, key] }.first(STATISTIC_ENTRIES)
      selected_keys = selected.to_h { |key, _| [key, true] }
      histogram = []
      unless oversized
        remaining = groups.reject { |key, _| selected_keys[key] }.sort_by(&:first).flat_map { |_, (value, frequency)| Array.new(frequency, value) }
        if remaining.length >= 2
          bounds = [HISTOGRAM_BOUNDS, remaining.length].min
          histogram = Array.new(bounds) { |index| remaining[index * (remaining.length - 1) / (bounds - 1)] }
        end
      end
      fact = { column: ordinal, analyzed_rows: rows.length, stale: false, null_count: null_count, width_sum: width_sum,
               distinct_count: distinct, sample_rows: sampled.length, sample_nonnull_rows: nonnull,
               mcv: selected.map { |_, pair| pair }, histogram: histogram }
      if term_eligible
        ranked = terms.map { |term, (count, _)| [term, count] }.sort_by { |term, count| [-count, term] }
        untracked = [ranked[STATISTIC_ENTRIES]&.last || 0,
                     pruned ? (occurrences + FileFormat::TERM_BUCKET_WIDTH - 1) / FileFormat::TERM_BUCKET_WIDTH : 0,
                     oversized_terms ? 1 : 0].max
        fact[:terms] = { occurrences: occurrences, untracked_rows: untracked, entries: ranked.first(STATISTIC_ENTRIES) }
      end
      fact
    end

    def statistics_row_snapshots
      @tables.filter_map { |name, stored| [name, stored[:rows]] unless stored[:storage_statistics].to_a.empty? }.to_h
    end

    def mark_changed_statistics(before)
      before.each do |name, rows|
        stored = @tables[name]
        mark_statistics_stale(name) if stored && !stored[:rows].equal?(rows)
      end
    end

    def mark_statistics_stale(name)
      stored = @tables[name]
      return unless stored && !stored[:storage_statistics].to_a.empty?
      @tables[name] = stored.merge(storage_statistics: stored[:storage_statistics].map { |fact| fact.merge(stale: true) }, storage_rows: stored[:rows])
    end

    def reindex(statement)
      if @transaction && (statement[:options][:concurrently] || %i[database system].include?(statement[:object]))
        raise Error.new('25001', 'REINDEX form cannot run inside a transaction block')
      end
      targets = reindex_targets(statement)
      replacements = []
      targets.each do |stored, definitions|
        next if definitions.empty?
        definitions.each { |index| secondary_metadata(stored, index) }
        definitions.each do |_index|
          charge(primary_page_count(stored))
          stored[:rows].each { charge }
        end
        codec = statistics_codec
        ordered, auxiliary = definitions.partition { |index| !index[:method] || index[:method] == 'btree' }
        metadata, indexes = build_secondary_indexes(stored, codec, ordered, fresh: true)
        auxiliary_indexes = build_auxiliary_indexes(stored, codec, auxiliary, fresh: true)
        replacements << [stored[:name], stored.merge(
          native_secondary_metadata: (stored[:native_secondary_metadata] || {}).merge(metadata).freeze,
          native_secondary_indexes: (stored[:native_secondary_indexes] || {}).merge(indexes).freeze,
          native_secondary_codec: codec,
          native_auxiliary_indexes: (stored[:native_auxiliary_indexes] || {}).merge(auxiliary_indexes).freeze,
          native_auxiliary_codec: codec, index_rebuild_generation: (stored[:index_rebuild_generation] || 0) + 1)]
      end
      replacements.each { |name, stored| @tables[name] = stored }
      @dirty = true unless replacements.empty?
      Result.new
    end

    def reindex_targets(statement)
      kind, name, database = statement.values_at(:object, :name, :database)
      if %i[database system].include?(kind)
        raise Error.new('0A000', 'cannot reindex system catalogs concurrently') if kind == :system && statement[:options][:concurrently]
        raise Error.new('0A000', 'can only reindex the currently open database') unless name.nil? || name == 'main'
        return [] if kind == :system
        return @tables.values.sort_by { |stored| [stored[:temporary] ? 1 : 0, stored[:name].b] }.map { |stored| [stored, catalog_indexes(stored)] }
      end
      introspection_scope(database) if database
      candidates = @tables.values.select { |stored| !database || !!stored[:temporary] == (database == 'temp') }
      if kind == :table && (stored = candidates.find { |candidate| candidate[:name] == name })
        return [[stored, catalog_indexes(stored)]]
      end
      if kind == :index
        candidates.each do |stored|
          index = catalog_indexes(stored).find { |candidate| candidate[:name] == name }
          return [[stored, [index]]] if index
        end
      end
      exists = candidates.any? { |stored| stored[:name] == name || catalog_indexes(stored).any? { |index| index[:name] == name } } || @views.key?(name) || @sequences.key?(name) || introspection_relation?(name)
      raise Error.new(exists ? '42809' : '42P01', "relation #{name} is not a #{kind}")
    end
  end

  Engine.prepend(Maintenance)
end
