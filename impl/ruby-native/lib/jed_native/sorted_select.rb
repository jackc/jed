# frozen_string_literal: true

module JedNative
  # A single-relation sort owns source rows. An ordinary LIMIT keeps a max
  # heap; collated ordering selects survivors before decorating retained rows.
  module SortedSelect
    def select(statement)
      return super unless sorted_select_shape?(statement)
      source = statement[:from].first
      stored = table(source[:name])
      resolved = resolved_read_plan(statement, stored)
      scope, projections, expressions, columns = resolved.scope, resolved.projections, resolved.expressions, resolved.columns
      return super if expressions.any? { |expression| aggregate_expr?(expression) }
      predicate = resolved.predicate
      access = scan_access_plan(stored, predicate, statement)
      return super unless access[:order] == :blocking
      orders = statement[:order_by]
      keys = orders.map do |order|
        node = order[:expr]
        if node[:kind] == :literal && node[:value].is_a?(Integer)
          raise Error.new('42P10', 'ORDER BY position out of range') unless node[:value].between?(1, expressions.length)
          expressions[node[:value] - 1]
        elsif node[:kind] == :column && !node[:table] && (index = columns.index { |column| column[:name] == node[:name] })
          matches = columns.each_index.select { |ordinal| columns[ordinal][:name] == node[:name] }
          if matches.map { |ordinal| projections[ordinal][:expr] }.uniq.length > 1
            raise Error.new('42702', 'ORDER BY name is ambiguous')
          end
          expressions[index]
        else
          compile(node, scope)
        end
      end
      keys.each { |expression| ensure_comparable_type(expression.type) }
      kernels = native_compiled_kernels(resolved, predicate, expressions, keys)
      predicate_kernel, projection_kernel, key_kernel = kernels&.predicate, kernels&.projection, kernels&.keys
      offset = statement[:offset] ? evaluate(compile(statement[:offset], []), []) || 0 : 0
      limit = statement[:limit] ? evaluate(compile(statement[:limit], []), []) : nil
      raise Error.new('2201X', 'negative OFFSET') if offset.negative?
      raise Error.new('2201W', 'negative LIMIT') if limit && limit.negative?
      capacity = limit && offset + limit
      collated = keys.any? { |expression| expression.instance_variable_get(:@native_collation)&.table }
      previous = [@native_stream_scan, @native_scan_columns]
      @native_stream_scan = true
      @native_scan_columns = scan_column_inventory(statement)
      scan_start = @cost if @explain_profile_frames
      input = execute_scan_access(stored, predicate, access)
      unless input
        charge(full_scan_pages(stored))
        charge_scan_payload(stored)
        ordinals = stored[:native_primary_entries]&.lazy&.map(&:last) || stored[:rows].each_index
        input = scan_rows(stored, ordinals)
      end
      profile_accumulate(:scan, @cost - scan_start) if @explain_profile_frames
      scan_resume = @cost if @explain_profile_frames
      touched = spill_mask(statement, scope) if spill_available?
      spill_sort = spill_available? && !collated && (!capacity || !keys.all? { |expression| expression.kind == :column } ||
        scope.each_with_index.any? { |column, index| touched[index] && !%w[i16 i32 i64 f32 f64 boolean date time timestamp timestamptz uuid interval].include?(column[:type]) } ||
        capacity * (8 + 40 * scope.length) > @work_mem)
      if @query_memory
        owner = query_memory_owner
        output_owner = query_memory_owner if query_memory_collect_result?
        mask = query_memory_mask(scope)
      end
      compare = lambda do |left, right|
        comparison = 0
        orders.each_with_index do |order, index|
          comparison = order_compare(left[1][index], right[1][index], order)
          break unless comparison.zero?
        end
        comparison.zero? ? left[2] <=> right[2] : comparison
      end
      rows = []
      if spill_sort
        added_keys = keys.each_index.reject { |index| keys[index].kind == :column }
        sorter = ExternalSorter.new(self, measure: ->(entry) { LogicalMemory.row_bytes(entry[0] + added_keys.map { |index| entry[1][index] }) }, &compare)
      end
      ordinal = 0
      unless limit == 0 && keys.all? { |expression| expression.kind == :column } && !collated && !spill_sort
        input.each do |row|
          begin
          charge
          profile_accumulate(:scan, @cost - scan_resume) if @explain_profile_frames
          filter_start = @cost if @explain_profile_frames
          accepted = !predicate || (predicate_kernel ? predicate_kernel.call(self, row, @params) : evaluate(predicate, row)) == true
          profile_accumulate(:filter, @cost - filter_start) if @explain_profile_frames
          next unless accepted
          sort_start = @cost if @explain_profile_frames
          values = key_kernel ? key_kernel.call(self, row, @params) : keys.map { |expression| evaluate(expression, row) }
          profile_accumulate(:sort, @cost - sort_start) if @explain_profile_frames
          next if capacity == 0
          entry = [row, values, ordinal]
          entry[0] = row.each_with_index.map { |value, index| touched[index] ? value : nil } if touched && !collated
          ordinal += 1
          if sorter
            sorter.push(entry)
            next
          end
          if capacity && !collated && rows.length >= capacity
            next unless compare.call(entry, rows.first).negative?
            if @query_memory
              query_memory_add_row(owner, row, scope, mask)
              query_memory_release(owner, query_memory_row_bytes(rows.first[0], scope, mask))
            end
            rows[0] = entry
            sort_heap_down(rows, 0, compare)
          else
            query_memory_add_row(owner, row, scope, mask) if @query_memory
            rows << entry
            sort_heap_up(rows, rows.length - 1, compare) if capacity && !collated
          end
          ensure
            scan_resume = @cost if @explain_profile_frames
          end
        end
      end
      if @explain_profile_frames
        profile_inherit(:filter, :scan)
        profile_inherit(:sort, predicate ? :filter : :scan)
      end
      if collated && !sorter
        if capacity
          query_memory_release(owner) if @query_memory
          heap = []
          decoration_owner = query_memory_owner if @query_memory
          rows.each do |entry|
            if heap.length < capacity
              entry << query_memory_collation_bytes(entry[1], decoration_owner) if @query_memory
              query_memory_add(decoration_owner, entry[3]) if @query_memory
              heap << entry
              sort_heap_up(heap, heap.length - 1, compare)
            elsif capacity.positive? && compare.call(entry, heap.first).negative?
              if @query_memory
                entry << query_memory_collation_bytes(entry[1], decoration_owner)
                query_memory_add(decoration_owner, entry[3])
                query_memory_release(decoration_owner, heap.first[3])
              end
              heap[0] = entry
              sort_heap_down(heap, 0, compare)
            end
          end
          rows = heap
          query_memory_release(decoration_owner) if @query_memory
          rows.each { |entry| query_memory_add_row(owner, entry[0], scope, mask) } if @query_memory
        else
          rows.each { |entry| query_memory_add(owner, query_memory_collation_bytes(entry[1], owner)) } if @query_memory
        end
      end
      rows.sort!(&compare) unless sorter
      ordered = sorter || rows
      output = cursor_output(columns)
      window = ordered.lazy.drop(offset)
      window = window.take(limit) if limit
      host_batch_rows(window, expressions, pulled: true, row_at: ->(entry) { entry[0] }).each do |entry|
        row = entry[0]
        values = projection_kernel ? projection_kernel.call(self, row, @params) : expressions.map { |expression| evaluate(expression, row) }
        query_memory_add_row(output_owner, values, columns) if output_owner
        charge
        output << values
      end
      profile_checkpoint(:root) if @explain_profile_frames
      result = Relation.new(columns: columns, rows: output)
      query_memory_attach(result, output_owner) if output_owner
      result
    ensure
      query_memory_release(owner) if @query_memory && owner
      sorter&.close
      @native_stream_scan, @native_scan_columns = previous if previous
    end

    private

    def sorted_select_shape?(statement)
      return false unless statement[:kind] == :select && !(statement[:order_by] || []).empty?
      return false if statement[:with] || statement[:distinct] || statement[:distinct_on] || statement[:having]
      return false unless (statement[:joins] || []).empty? && (statement[:group_by] || []).empty? && (statement[:windows] || {}).empty?
      sources = statement[:from] || []
      return false unless sources.length == 1 && sources[0][:kind] == :table && @tables.key?(sources[0][:name]) && !@ctes.key?(sources[0][:name])
      return false if sources[0][:columns] || sources[0][:column_aliases]
      !contains_window?([statement[:columns], statement[:order_by]]) && !contains_aggregate?(statement[:columns])
    end

    def sort_heap_up(rows, index, compare)
      while index.positive?
        parent = (index - 1) / 2
        break unless compare.call(rows[index], rows[parent]).positive?
        rows[index], rows[parent] = rows[parent], rows[index]
        index = parent
      end
    end

    def sort_heap_down(rows, index, compare)
      loop do
        child = index * 2 + 1
        break if child >= rows.length
        child += 1 if child + 1 < rows.length && compare.call(rows[child + 1], rows[child]).positive?
        break unless compare.call(rows[child], rows[index]).positive?
        rows[index], rows[child] = rows[child], rows[index]
        index = child
      end
    end
  end
  Engine.prepend(SortedSelect)
end
