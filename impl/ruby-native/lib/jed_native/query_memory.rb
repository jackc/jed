# frozen_string_literal: true

module JedNative
  # Logical ownership, deliberately independent of Ruby's allocator and GC.
  # Tokens move with buffers; releasing a token twice is harmless.
  class QueryMemoryAccount
    Owner = Struct.new(:bytes)
    attr_reader :used, :peak, :limit

    def initialize(limit, pending = 0)
      @limit, @used, @peak, @pending = limit, pending, pending, pending
    end

    def reserve(bytes)
      return false if bytes > @limit - @used
      @used += bytes
      @peak = @used if @used > @peak
      true
    end

    def release(bytes)
      @used = [@used - bytes, @pending].max
    end

    def pending=(bytes)
      @used += bytes - @pending
      @pending = bytes
      @peak = @used if @used > @peak
    end
  end

  module QueryMemory
    attr_accessor :max_query_memory_bytes
    attr_reader :query_memory_peak

    def initialize(max_query_memory_bytes: 0, **options)
      @max_query_memory_bytes = max_query_memory_bytes
      @query_memory_pending = 0
      @query_memory_versions = {}
      super(**options)
    end

    def run(statement, params = [])
      @query_memory = QueryMemoryAccount.new(@max_query_memory_bytes, @query_memory_pending) if @max_query_memory_bytes.positive?
      @query_memory_contexts = [] if @query_memory
      @query_memory_aggregate_owner = nil
      super
    ensure
      @query_memory_peak = @query_memory&.peak || 0
      @query_memory = nil
      @query_memory_contexts = nil
      unless @transaction
        @query_memory_pending = 0
        @query_memory_versions = {}
      end
    end

    def snapshot
      super + [{ native_memory_snapshot: true, pending: @query_memory_pending, versions: @query_memory_versions.dup }]
    end

    def restore(state)
      if state.last.is_a?(Hash) && state.last[:native_memory_snapshot]
        memory = state.last
        @query_memory_pending = memory[:pending]
        @query_memory_versions = memory[:versions].dup
        @query_memory.pending = @query_memory_pending if @query_memory
        super(state[0...-1])
      else
        super
      end
    end

    def dispatch(statement)
      result = super
      if %i[savepoint rollback_to release].include?(statement[:kind])
        # A savepoint's older versions remain live even when its current key is replaced.
        @query_memory_versions = {}
      elsif %i[commit rollback].include?(statement[:kind])
        @query_memory_pending, @query_memory_versions = 0, {}
        @query_memory.pending = 0 if @query_memory
      end
      query_memory_guard if @query_memory && @query_memory.used > @query_memory.limit
      result
    end

    def insert(statement)
      previous = @query_memory_materialize
      @query_memory_materialize = true if @query_memory && statement[:query]
      super
    ensure
      @query_memory_materialize = previous
    end

    def project_mutation(statement, table, operation, changes, scope, expressions, columns)
      result = super
      query_memory_stage_changes(table, changes)
      result
    end

    def create_table(statement)
      existed = @tables.key?(statement[:name])
      result = super
      stored = @tables[statement[:name]]
      if !existed && stored && !stored[:rows].empty?
        query_memory_stage_changes(stored.merge(rows: [], rowids: [], next_rowid: 1), stored[:rows].map { |row| [nil, row] })
      end
      result
    end

    def alter_table(statement)
      original = @tables[statement[:name]]
      result = super
      name = statement[:action] == :rename ? statement[:new_name] : statement[:name]
      stored = @tables[name]
      return result unless original && stored
      if name != original[:name]
        @query_memory_versions = @query_memory_versions.to_h do |key, bytes|
          [key.first == original[:name] ? [name, *key.drop(1)] : key, bytes]
        end
      end
      rewrite = (statement[:actions] || []).any? do |action|
        %i[add_column drop_column set_type drop_primary_key].include?(action[:kind]) || action[:kind] == :add_constraint && action.dig(:constraint, :kind) == :primary_key
      end
      if rewrite
        codec = statistics_codec
        original[:rows].each_with_index do |row, index|
          next if stored[:temporary]
          query_memory_replace_records(original[:name], query_memory_storage_records(original, row, original[:rowids][index], codec), {})
        end
        stored[:rows].each_with_index do |row, index|
          records = query_memory_storage_records(stored, row, stored[:rowids][index], codec, meter: true)
          query_memory_replace_records(name, {}, records) unless stored[:temporary]
        end
      else
        query_memory_stage_new_indexes(original, stored) unless stored[:temporary]
      end
      query_memory_check_pending
      result
    end

    def create_index(statement)
      original = @tables[statement[:table]]
      result = super
      stored = @tables[statement[:table]]
      query_memory_stage_new_indexes(original, stored) if original && stored && !stored[:temporary]
      query_memory_check_pending
      result
    end

    def drop_index(statement)
      before = @tables.dup
      result = super
      before.each do |name, original|
        stored = @tables[name]
        next unless stored
        removed = catalog_indexes(original).map { |index| index[:name] } - catalog_indexes(stored).map { |index| index[:name] }
        @query_memory_versions.delete_if do |key, bytes|
          dropped = key[0] == name && key[1] == :index && removed.include?(key[2])
          @query_memory_pending -= bytes if dropped
          dropped
        end
      end
      query_memory_check_pending
      result
    end

    def truncate(statement)
      result = super
      query_memory_forget_tables(statement[:names] || [statement[:name] || statement[:table]])
      result
    end

    def drop_table(statement)
      names = statement[:names] || [statement[:name]]
      result = super
      query_memory_forget_tables(names)
      result
    end

    def explain_query(statement)
      result = super
      if @query_memory
        owner = query_memory_owner
        columns = result.column_types.map { |type| { type: type } }
        result.rows.each { |row| query_memory_add_row(owner, row, columns) }
      end
      result
    end

    def select(statement)
      return super unless @query_memory
      context = { statement: statement, depth: @query_memory_contexts.length, owners: [] }
      context[:vector_group] = query_memory_vector_group?(statement)
      @query_memory_contexts << context
      result = super
      result
    ensure
      if context
        output = result&.instance_variable_get(:@native_memory_owner)
        context[:owners].each { |owner| query_memory_release(owner) unless owner.equal?(output) }
        @query_memory_contexts.pop
        @query_memory_contexts.last&.dig(:owners)&.push(output) if output
      end
    end

    def source_relation(source)
      relation = super
      return relation unless @query_memory
      mask = query_memory_mask(relation.columns)
      binding = source[:kind] == :table && @ctes[source[:name]]
      if binding && (!binding.respond_to?(:query) || !binding.query)
        # A materialized CTE owns its body and every reference owns a copy.
        query_memory_relation(relation, mask: mask, copy: true)
      else
        query_memory_relation(relation, mask: mask)
      end
    end

    def query_memory_owner
      owner = QueryMemoryAccount::Owner.new(0)
      @query_memory_contexts&.last&.dig(:owners)&.push(owner)
      owner
    end

    def query_memory_add(owner, bytes)
      return if bytes.zero?
      unless @query_memory.reserve(bytes)
        query_memory_guard
      end
      owner.bytes += bytes
    end

    def query_memory_add_row(owner, row, columns = nil, mask = nil)
      query_memory_add(owner, query_memory_row_bytes(row, columns, mask))
    end

    def query_memory_row_bytes(row, columns = nil, mask = nil)
      bytes = MemorySchedule::ROW + row.length * MemorySchedule::VALUE
      row.each_with_index do |value, index|
        bytes += LogicalMemory.payload(value, columns && columns[index]&.dig(:type)) if !mask || mask[index]
      end
      bytes
    end

    def query_memory_key_bytes(values, types = nil)
      values.each_with_index.sum { |value, index| LogicalMemory.value_bytes(value, types && types[index]) }
    end

    def query_memory_collation_bytes(values, owner = nil)
      keys = values.filter_map do |value|
        label = CollationRules.label(value)
        Bytea.new(label.table.sort_key(value)) if !value.nil? && label&.table
      end
      if owner && !keys.empty?
        decorations = owner.instance_variable_get(:@native_collation_decorations) || []
        decorations.concat(keys)
        owner.instance_variable_set(:@native_collation_decorations, decorations)
      end
      keys.sum { |key| LogicalMemory.value_bytes(key, 'bytea') }
    end

    def query_memory_release(owner, bytes = nil)
      owner = owner.instance_variable_get(:@native_memory_owner) if owner.is_a?(Relation)
      return unless owner && owner.bytes.positive?
      amount = bytes ? [bytes, owner.bytes].min : owner.bytes
      @query_memory.release(amount)
      owner.bytes -= amount
      owner.remove_instance_variable(:@native_collation_decorations) if owner.bytes.zero? && owner.instance_variable_defined?(:@native_collation_decorations)
    end

    def query_memory_attach(relation, owner, mask = nil)
      relation.instance_variable_set(:@native_memory_owner, owner)
      relation.instance_variable_set(:@native_memory_mask, mask)
      relation
    end

    def query_memory_transfer(from, to)
      query_memory_attach(to, from.instance_variable_get(:@native_memory_owner), from.instance_variable_get(:@native_memory_mask))
    end

    def query_memory_relation(relation, mask: nil, copy: false)
      return relation if @query_memory_contexts&.last&.dig(:vector_group)
      existing = relation.instance_variable_get(:@native_memory_owner)
      return relation if existing && !copy
      owner = query_memory_owner
      relation.rows.each { |row| query_memory_add_row(owner, row, relation.columns, mask) }
      query_memory_attach(relation, owner, mask)
    end

    def query_memory_filter(relation)
      owner = relation.instance_variable_get(:@native_memory_owner)
      mask = relation.instance_variable_get(:@native_memory_mask)
      rows = relation.rows.select do |row|
        accepted = yield row
        query_memory_release(owner, query_memory_row_bytes(row, relation.columns, mask)) unless accepted
        accepted
      end
      query_memory_transfer(relation, Relation.new(columns: relation.columns, rows: rows))
    end

    def query_memory_collect_result?
      @query_memory_contexts.length > 1 || @query_memory_materialize || @query_memory_contexts.last&.dig(:statement, :with)
    end

    def query_memory_mask(columns)
      statement = @query_memory_contexts&.last&.dig(:statement)
      return Array.new(columns.length, true) unless statement
      mask = Array.new(columns.length, false)
      visit = lambda do |node|
        case node
        when Array then node.each { |child| visit.call(child) }
        when Hash
          next if node[:kind] == :call && node[:name] == 'count' && node[:args]&.all? { |argument| argument[:kind] == :star }
          if %i[column star].include?(node[:kind])
            columns.each_with_index do |column, index|
              mask[index] = true if (!node[:table] || node[:table] == column[:table]) && (node[:kind] == :star || node[:name] == column[:name])
            end
          else
            node.each_value { |child| visit.call(child) }
          end
        end
      end
      visit.call(statement)
      mask
    end

    def query_memory_guard
      raise Error.new('54P01', 'statement cost limit exceeded') if @max_cost.positive? && @cost >= @max_cost
      raise Error.new('54P02', 'session cost limit exceeded') if @lifetime_max_cost.positive? && @lifetime_cost >= @lifetime_max_cost
      raise Error.new('54P05', "query memory exceeded the limit of #{@query_memory.limit} bytes")
    end

    def query_memory_vector_group?(statement)
      return false unless statement[:kind] == :select && !statement[:with] && (statement[:joins] || []).empty?
      sources = statement[:from] || []
      return false unless sources.length == 1 && sources.first[:kind] == :table && (stored = @tables[sources.first[:name]])
      aggregates = projection_aggregates([statement[:columns], statement[:having], statement[:order_by]])
      return false if aggregates.empty? || !window_nodes(statement[:columns]).empty?
      return false unless aggregates.all? { |node| %w[count sum avg min max bool_and bool_or every bit_and bit_or bit_xor].include?(node[:name]) && !node[:distinct] && !node[:filter] && (node[:order_by] || []).empty? }
      groups = statement[:group_by] || []
      groups.empty? || groups.length == 1 && groups[0][:kind] == :column && stored[:columns].any? { |column| column[:name] == groups[0][:name] && Types::INTEGER_BITS.key?(column[:type]) }
    end

    def query_memory_stage_changes(table, changes)
      return if changes.empty?
      codec = statistics_codec
      paged = table[:rows].is_a?(PagedRows) || table[:rows].is_a?(PendingRows)
      primary = table[:constraints].any? { |constraint| constraint[:kind] == :primary_key }
      positions = table[:rows].each_with_index.to_h { |row, index| [row.object_id, index] } unless paged
      next_id = table[:next_rowid]
      removed, added = {}, {}
      changes.each do |before, after|
        rowid = if !before
          next_id
        elsif table[:rows].is_a?(PendingRows)
          table[:rows].rowid_for(before)
        elsif paged && before.respond_to?(:storage_key)
          primary ? 0 : codec.decode_int(8, before.storage_key)
        else
          table[:rowids][positions.fetch(before.object_id)]
        end
        next_id += 1 unless before
        previous = before && !@query_memory_versions.empty? ? query_memory_storage_records(table, before, rowid, codec) : {}
        current = after ? query_memory_storage_records(table, after, rowid, codec, meter: true) : {}
        removed.merge!(previous)
        added.merge!(current)
      end
      return if table[:temporary]
      query_memory_replace_records(table[:name], removed, added)
      query_memory_check_pending
    end

    def query_memory_check_pending
      if @query_memory
        @query_memory.pending = @query_memory_pending
        query_memory_guard if @query_memory.used > @query_memory.limit
      end
    end

    def query_memory_replace_records(name, previous, current)
      previous.each_key { |key| @query_memory_pending -= @query_memory_versions.delete([name, *key]) || 0 }
      current.each do |key, bytes|
        @query_memory_pending += bytes
        @query_memory_versions[[name, *key]] = bytes if @query_memory
      end
    end

    def query_memory_forget_tables(names)
      @query_memory_versions.delete_if do |key, bytes|
        removed = names.include?(key[0])
        @query_memory_pending -= bytes if removed
        removed
      end
      query_memory_check_pending
    end

    def query_memory_stage_new_indexes(original, stored)
      existing = catalog_indexes(original).map { |index| index[:name] }
      definitions = catalog_indexes(stored).reject { |index| existing.include?(index[:name]) }
      return if definitions.empty?
      codec = statistics_codec
      gist, others = definitions.partition { |index| index[:method] == 'gist' }
      # Q3 measures encoded leaf entries, not the routing structure. Resolve
      # each definition once and avoid building a singleton GiST per row.
      cost, lifetime, maximum, lifetime_maximum = @cost, @lifetime_cost, @max_cost, @lifetime_max_cost
      @max_cost = @lifetime_max_cost = 0
      gist = gist.map do |index|
        details = secondary_metadata(stored, index)
        operations = codec.gist_opclasses_for(stored, cols: details[:columns])
        [index[:name], details, operations]
      end
      stored[:rows].each_with_index do |row, ordinal|
        records = others.empty? ? {} : query_memory_index_records(stored.merge(rows: [row], rowids: [stored[:rowids][ordinal]]), codec, others)
        unless gist.empty?
          key = row.respond_to?(:storage_key) ? row.storage_key : query_memory_storage_key(stored, row, stored[:rowids][ordinal], codec)
          gist.each do |name, details, operations|
            next if details[:columns].any? { |column| row[column].nil? }
            next if details[:predicate] && evaluate(details[:predicate], row) != true
            values = Array.new(stored[:columns].length)
            details[:columns].each { |column| values[column] = statistics_value(stored[:columns][column][:type], row[column]) }
            bound = codec.gist_leaf_bound(operations, values, details[:columns])
            encoded = codec.gist_encode_bound(operations, bound)
            records[[:index, name, encoded + key]] = 4 + encoded.bytesize + key.bytesize
          end
        end
        query_memory_replace_records(stored[:name], {}, records)
      end
    ensure
      @cost, @lifetime_cost, @max_cost, @lifetime_max_cost = cost, lifetime, maximum, lifetime_maximum if defined?(cost) && cost
    end

    def query_memory_storage_key(table, row, rowid, codec)
      statistics_storage_rows(table.merge(rows: [row], rowids: [rowid]), codec).first.first
    end

    def query_memory_storage_records(table, row, rowid, codec, meter: false)
      key = query_memory_storage_key(table, row, rowid, codec)
      values, forms, compressed, bytes = storage_record_plan(table, key, row, codec, storage_capacity(table), meter: meter)
      forms.each_with_index do |form, index|
        if form == :external
          value = values[index]
          bytes += value.is_a?(StoredValue) ? value.payload_length : codec.value_payload(table[:columns][index][:type], value).bytesize
        end
        bytes += compressed[index].bytesize if form == :external_comp
      end
      records = { [:primary, key] => bytes }
      records.merge!(query_memory_index_records(table.merge(rows: [row], rowids: [rowid]), codec)) if table[:indexes].any? || table[:constraints].any? { |constraint| %i[unique exclude].include?(constraint[:kind]) }
      records
    end

    def query_memory_index_records(table, codec, definitions = nil)
      definitions ||= catalog_indexes(table)
      records = {}
      btrees, auxiliary = definitions.partition { |definition| !definition[:method] || definition[:method] == 'btree' }
      # This table contains only staged versions. Q3 needs their entry bytes;
      # applying them as a replacement for the committed index would walk and
      # delete every unchanged key merely to size one row.
      _, indexes = build_secondary_indexes(table, codec, btrees, fresh: true)
      indexes.each do |name, entries|
        entries.each { |key, _| records[[:index, name, key]] = key.bytesize }
      end
      unless auxiliary.empty?
        build_auxiliary_indexes(table, codec, auxiliary, fresh: true).each do |name, details|
          if details[:kind] == 'gin'
            details[:entries].each { |key, _| records[[:index, name, key]] = key.bytesize }
          else
            details[:entries].each do |bound, key|
              encoded = codec.gist_encode_bound(details[:opclasses], bound)
              records[[:index, name, encoded + key]] = 4 + encoded.bytesize + key.bytesize
            end
          end
        end
      end
      records
    end
  end
  Engine.prepend(QueryMemory)
end
