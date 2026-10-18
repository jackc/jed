# frozen_string_literal: true

module JedNative
  module StreamingSelect
    def select(statement)
      return super unless streaming_select_shape?(statement)
      source = statement[:from].first
      stored = table(source[:name])
      scope = table_columns(stored, source[:alias] || source[:name])
      projections = expand_stars(statement[:columns], scope)
      expressions = projections.map { |entry| compile(entry[:expr], scope) }
      return super if expressions.any? { |expression| aggregate_expr?(expression) }
      columns = projections.zip(expressions).map { |entry, expression| { name: entry[:alias] || expression_name(entry[:expr]), type: expression.type } }
      predicate = compile_filter(statement[:where], scope)
      access = scan_access_plan(stored, predicate, statement)
      order = access[:order]
      return super if order == :blocking
      offset = statement[:offset] ? evaluate(compile(statement[:offset], []), []) || 0 : 0
      limit = statement[:limit] ? evaluate(compile(statement[:limit], []), []) : nil
      raise Error.new('2201X', 'negative OFFSET') if offset.negative?
      raise Error.new('2201W', 'negative LIMIT') if limit && limit.negative?
      previous = [@native_stream_scan, @native_reverse_scan, @native_scan_columns]
      @native_stream_scan, @native_reverse_scan = true, order == :reverse
      @native_scan_columns = scan_column_inventory(statement)
      scan_start = @cost if @explain_profile_frames
      input = execute_scan_access(stored, predicate, access)
      unless input
        charge(full_scan_pages(stored))
        charge_scan_payload(stored)
        ordinals = stored[:native_primary_entries]&.map(&:last) || stored[:rows].each_index.to_a
        input = scan_rows(stored, ordinals)
      end
      profile_accumulate(:scan, @cost - scan_start) if @explain_profile_frames
      scan_resume = @cost if @explain_profile_frames
      output = []
      memory_owner = query_memory_owner if @query_memory && query_memory_collect_result?
      unless limit == 0
        input.each do |row|
          begin
            charge
            profile_accumulate(:scan, @cost - scan_resume) if @explain_profile_frames
            filter_start = @cost if @explain_profile_frames
            accepted = !predicate || evaluate(predicate, row) == true
            profile_accumulate(:filter, @cost - filter_start) if @explain_profile_frames
            next unless accepted
            if offset.positive?
              offset -= 1
              next
            end
            result_row = expressions.map { |expression| evaluate(expression, row) }
            query_memory_add_row(memory_owner, result_row, columns) if memory_owner
            output << result_row
            charge
            break if limit && output.length >= limit
          ensure
            scan_resume = @cost if @explain_profile_frames
          end
        end
      end
      profile_inherit(:filter, :scan) if @explain_profile_frames
      profile_checkpoint(:root) if @explain_profile_frames
      result = Relation.new(columns: columns, rows: output)
      query_memory_attach(result, memory_owner) if memory_owner
      result
    ensure
      @native_stream_scan, @native_reverse_scan, @native_scan_columns = previous if previous
    end

    def streaming_select_shape?(statement)
      return false unless statement[:kind] == :select && !statement[:with] && !statement[:distinct] && !statement[:distinct_on] && !statement[:having]
      return false unless (statement[:joins] || []).empty? && (statement[:group_by] || []).empty? && (statement[:windows] || {}).empty?
      sources = statement[:from] || []
      return false unless sources.length == 1 && sources[0][:kind] == :table && @tables.key?(sources[0][:name]) && !@ctes.key?(sources[0][:name])
      return false if sources[0][:columns] || sources[0][:column_aliases]
      return false unless statement[:limit] || statement[:offset] || !(statement[:order_by] || []).empty?
      !contains_window?(statement[:columns]) && !contains_aggregate?(statement[:columns])
    end

    private

    def stream_order(statement, stored, scope, projections, columns)
      orders = statement[:order_by] || []
      return :none if orders.empty?
      primary = stored.dig(:native_primary_metadata, :columns) || []
      return :blocking if primary.empty? || orders.length > primary.length
      directions = orders.map { |order| order[:direction] == :desc }
      return :blocking unless directions.uniq.length == 1 && (!directions[0] || orders.length == primary.length)
      valid = orders.each_with_index.all? do |order, i|
        ast = order[:expr]
        expression = if ast[:kind] == :literal && ast[:value].is_a?(Integer)
          projections[ast[:value] - 1]
        elsif ast[:kind] == :column && !ast[:table] && (ordinal = columns.index { |column| column[:name] == ast[:name] })
          projections[ordinal]
        else
          compile(ast, scope)
        end
        expression&.kind == :column && expression.value == primary[i]
      end
      valid ? (directions[0] ? :reverse : :forward) : :blocking
    end

    def contains_window?(node)
      case node
      when Array then node.any? { |child| contains_window?(child) }
      when Hash then !!node[:over] || node.each_value.any? { |child| contains_window?(child) }
      else false
      end
    end
  end
  Engine.prepend(StreamingSelect)
end
