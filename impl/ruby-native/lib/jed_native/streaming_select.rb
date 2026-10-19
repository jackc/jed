# frozen_string_literal: true

module JedNative
  module StreamingSelect
    def select(statement)
      return super unless streaming_select_shape?(statement)
      # A cursor alone must not move a budgeted query expression onto the
      # streaming lane: the ordinary executor charges output before projection,
      # and an inner query may finish exactly at an unguarded output checkpoint.
      # Check syntax before resolving, which can execute independent subqueries.
      if (@max_cost.positive? || @lifetime_max_cost.positive?) &&
         !statement[:limit] && !statement[:offset] && (statement[:order_by] || []).empty? &&
         streaming_query_projection?(statement[:columns])
        return super
      end
      source = statement[:from].first
      stored = table(source[:name])
      resolved = resolved_read_plan(statement, stored)
      scope, projections, expressions, columns = resolved.scope, resolved.projections, resolved.expressions, resolved.columns
      return super if expressions.any? { |expression| aggregate_expr?(expression) }
      validate_aggregate_scope(statement, scope, false) if statement[:distinct]
      predicate = resolved.predicate
      # Budget/profile lanes retain their original incremental charge schedule.
      point = resolved.point unless @explain_profile_frames || @max_cost.positive? || @lifetime_max_cost.positive? || @query_memory
      access = scan_access_plan(stored, predicate, statement) unless point
      order = point ? :none : access[:order]
      return super if order == :blocking
      pipeline_mode = :stream unless resolved.point || statement[:distinct] || @query_memory ||
                                      @explain_profile_frames || @native_pipeline_disabled
      kernels = native_compiled_kernels(resolved, predicate, expressions, pipeline: pipeline_mode,
        offset: !!statement[:offset], limit: !!statement[:limit])
      pipeline = kernels&.pipeline if pipeline_mode
      predicate_kernel, projection_kernel = kernels&.predicate, kernels&.projection
      offset = statement[:offset] ? evaluate(compile(statement[:offset], []), []) || 0 : 0
      limit = statement[:limit] ? evaluate(compile(statement[:limit], []), []) : nil
      raise Error.new('2201X', 'negative OFFSET') if offset.negative?
      raise Error.new('2201W', 'negative LIMIT') if limit && limit.negative?
      previous = [@native_stream_scan, @native_reverse_scan, @native_scan_columns, @native_ordered_scan]
      @native_stream_scan, @native_reverse_scan = true, order == :reverse
      @native_ordered_scan = %i[forward reverse].include?(order)
      @native_scan_columns = resolved.scan_columns
      scan_start = @cost if @explain_profile_frames
      input = point ? prepared_stream_point_rows(stored, point) : execute_scan_access(stored, predicate, access)
      unless input
        if %i[forward reverse].include?(order)
          input = storage_stream_rows(stored, reverse: order == :reverse)
        else
          charge(full_scan_pages(stored))
          charge_scan_payload(stored)
          ordinals = stored[:native_primary_entries]&.lazy&.map(&:last) || stored[:rows].each_index
          input = scan_rows(stored, ordinals)
        end
      end
      profile_accumulate(:scan, @cost - scan_start) if @explain_profile_frames
      scan_resume = @cost if @explain_profile_frames
      output = cursor_output(columns)
      memory_owner = query_memory_owner if @query_memory && query_memory_collect_result?
      if statement[:distinct]
        seen = spill_available? ? SpillStateMap.new(self) : {}
        seen_owner = query_memory_owner if @query_memory && seen.is_a?(Hash)
      end
      unless limit == 0
        if pipeline
          pipeline.call(self, input, @params, output, offset, limit)
        else
          remaining = limit && -> { offset + limit - output.length }
          host_batch_rows(input, [predicate, *expressions], pulled: true, remaining: remaining).each do |row|
            begin
              charge
              profile_accumulate(:scan, @cost - scan_resume) if @explain_profile_frames
              filter_start = @cost if @explain_profile_frames
              accepted = !predicate || (predicate_kernel ? predicate_kernel.call(self, row, @params) : evaluate(predicate, row)) == true
              profile_accumulate(:filter, @cost - filter_start) if @explain_profile_frames
              next unless accepted
              if seen
                result_row = projection_kernel ? projection_kernel.call(self, row, @params) : expressions.map { |expression| evaluate(expression, row) }
                if seen.is_a?(Hash)
                  next if seen.key?(result_row)
                  query_memory_add(seen_owner, MemorySchedule::ENTRY + query_memory_key_bytes(result_row)) if seen_owner
                  seen[result_row] = true
                else
                  next unless seen.insert(result_row)
                end
              end
              if offset.positive?
                offset -= 1
                next
              end
              result_row = (projection_kernel ? projection_kernel.call(self, row, @params) : expressions.map { |expression| evaluate(expression, row) }) unless seen
              query_memory_add_row(memory_owner, result_row, columns) if memory_owner
              charge
              output << result_row
              break if limit && output.length >= limit
            ensure
              scan_resume = @cost if @explain_profile_frames
            end
          end
        end
      end
      profile_inherit(:filter, :scan) if @explain_profile_frames
      profile_checkpoint(:root) if @explain_profile_frames
      result = Relation.new(columns: columns, rows: output)
      query_memory_attach(result, memory_owner) if memory_owner
      result
    ensure
      seen.close if seen && !seen.is_a?(Hash)
      query_memory_release(seen_owner) if seen_owner
      @native_stream_scan, @native_reverse_scan, @native_scan_columns, @native_ordered_scan = previous if previous
    end

    def streaming_select_shape?(statement)
      @native_stream_shapes ||= {}.compare_by_identity
      unless @native_stream_shapes.key?(statement)
        @native_stream_shapes.shift if @native_stream_shapes.length >= 64
        @native_stream_shapes[statement] = streaming_select_syntax?(statement)
      end
      return false unless @native_stream_shapes[statement]
      name = statement[:from][0][:name]
      return false unless @tables.key?(name) && !@ctes.key?(name)
      return false unless statement[:limit] || statement[:offset] || !(statement[:order_by] || []).empty? || cursor_output?
      true
    end

    private

    def streaming_query_projection?(node)
      pending = [node]
      until pending.empty?
        current = pending.pop
        case current
        when Array
          current.each { |child| pending << child if child.is_a?(Array) || child.is_a?(Hash) }
        when Hash
          return true if current.key?(:query)
          current.each_value { |child| pending << child if child.is_a?(Array) || child.is_a?(Hash) }
        end
      end
      false
    end

    def streaming_select_syntax?(statement)
      return false unless statement[:kind] == :select && !statement[:with] && !statement[:distinct_on] && !statement[:having]
      return false if statement[:distinct] && (statement[:order_by] || []).empty?
      return false unless (statement[:joins] || []).empty? && (statement[:group_by] || []).empty? && (statement[:windows] || {}).empty?
      sources = statement[:from] || []
      return false unless sources.length == 1 && sources[0][:kind] == :table
      return false if sources[0][:columns] || sources[0][:column_aliases]
      !contains_window?(statement[:columns]) && !contains_aggregate?(statement[:columns])
    end

    def prepared_stream_point_rows(stored, point)
      value = evaluate(point.expression, [])
      return [] if value.nil?
      return [] unless value.between?(point.minimum, point.maximum)
      charge(point.pages) unless point.pages.zero?
      row = point.index[value]
      return [] unless row
      ordinal = row.is_a?(PagedRow) ? row : stored[:native_row_positions]&.[](row.object_id)
      charge_scan_payload(stored, [ordinal]) if ordinal
      [row]
    end

    def stream_order(statement, stored, scope, projections, columns)
      orders = statement[:order_by] || []
      return :none if orders.empty?
      primary = stored.dig(:native_primary_metadata, :columns) || []
      return :blocking if primary.empty?
      directions = orders.take(primary.length).map { |order| order[:direction] == :desc }
      return :blocking unless directions.uniq.length == 1 && (!directions[0] || orders.length >= primary.length)
      resolved = orders.map do |order|
        ast = order[:expr]
        expression = if ast[:kind] == :literal && ast[:value].is_a?(Integer)
          projections[ast[:value] - 1]
        elsif ast[:kind] == :column && !ast[:table] && (ordinal = columns.index { |column| column[:name] == ast[:name] })
          projections[ordinal]
        else
          compile(ast, scope)
        end
        expression
      end
      valid = resolved.take(primary.length).each_with_index.all? { |expression, i| expression&.kind == :column && expression.value == primary[i] }
      valid ? (directions[0] ? :reverse : :forward) : :blocking
    end

    def contains_window?(node)
      return false unless node.is_a?(Array) || node.is_a?(Hash)
      @native_window_shapes ||= {}.compare_by_identity
      return @native_window_shapes[node] if @native_window_shapes.key?(node)
      result = window_shape?(node)
      @native_window_shapes.shift if @native_window_shapes.length >= 64
      @native_window_shapes[node] = result
      result
    end

    def window_shape?(node)
      pending = [node]
      until pending.empty?
        current = pending.pop
        case current
        when Array
          current.each { |child| pending << child if child.is_a?(Array) || child.is_a?(Hash) }
        when Hash
          return true if current[:over]
          current.each_value { |child| pending << child if child.is_a?(Array) || child.is_a?(Hash) }
        end
      end
      false
    end
  end
  Engine.prepend(StreamingSelect)
end
