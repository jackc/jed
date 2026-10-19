# frozen_string_literal: true

module JedNative
  # The common single-relation lane keeps resolved offsets and parameter sites
  # beside a parsed statement. A table object is immutable after publication;
  # replacing it on any write invalidates this plan and its lookup map together.
  # Other relational shapes continue through the general executor.
  module PreparedSelect
    Plan = Struct.new(:table, :source, :columns, :projections, :predicate, :orders,
                      :parameters, :parameter_types, :lookup, :index, :context, keyword_init: true)
    ReadPlan = Struct.new(:table, :scope, :projections, :expressions, :columns, :predicate,
                          :parameters, :context, :scan_columns, :point, keyword_init: true)
    PointRead = Struct.new(:expression, :index, :minimum, :maximum, :pages, keyword_init: true)
    CACHE_ENTRIES = 64
    STATEFUL_FUNCTIONS = %w[nextval currval setval lastval current_setting now
                            current_date current_time localtime localtimestamp
                            clock_timestamp uuidv4 gen_random_uuid uuidv7 random].freeze

    # Build one lookup map per published table, at the write/import boundary.
    # Plans share it; preparing another SQL string must not scan the table or
    # retain another full copy of the same index.
    def refresh_primary_indexes
      @tables.each do |name, stored|
        signature = [stored[:rows].object_id, stored[:columns].object_id, stored[:constraints].object_id]
        next if stored[:native_primary_signature] == signature
        primary = stored[:constraints].find { |constraint| constraint[:kind] == :primary_key }
        ordinal = if primary && primary[:columns].length == 1
          stored[:columns].index { |column| column[:name] == primary[:columns][0] }
        end
        ordinal = nil unless ordinal && Types::INTEGER_BITS.key?(stored[:columns][ordinal][:type])
        index = ordinal && stored[:rows].to_h { |row| [row[ordinal], row] }.freeze
        @tables[name] = stored.merge(native_primary_signature: signature,
                                    native_primary_ordinal: ordinal, native_primary_index: index,
                                    native_primary_sorted: index && index.sort_by(&:first).freeze)
      end
      # PagedAccess validates plans after restoring both resident and paged
      # tables; paged tables are temporarily absent during this method.
    end

    # A resolved integer primary-key bound is safe to test before reading any
    # stored row. Residual expressions are still evaluated on every candidate.
    def primary_scan(stored, predicate)
      ordinal = stored[:native_primary_ordinal]
      return nil unless ordinal && predicate
      bounds = []
      visit = lambda do |expression|
        if expression.kind == :binary && expression.value == 'and'
          expression.args.each { |arg| visit.call(arg) }
        elsif expression.kind == :binary && %w[= < <= > >=].include?(expression.value)
          left, right = expression.args
          operator = expression.value
          if right.kind == :column
            left, right = right, left
            operator = { '<' => '>', '<=' => '>=', '>' => '<', '>=' => '<=', '=' => '=' }.fetch(operator)
          end
          if left.kind == :column && left.value == ordinal && %i[literal param outer].include?(right.kind) && Types::INTEGER_BITS.key?(right.type)
            bounds << [operator, evaluate(right, [])]
          end
        end
      end
      visit.call(predicate)
      return nil if bounds.empty?
      return [] if bounds.any? { |_, value| value.nil? }
      minimum, maximum, equal = nil, nil, nil
      bounds.each do |operator, value|
        case operator
        when '='
          return [] if equal && equal != value
          equal = value
        when '>', '>='
          value += 1 if operator == '>'
          minimum = minimum ? [minimum, value].max : value
        when '<', '<='
          value -= 1 if operator == '<'
          maximum = maximum ? [maximum, value].min : value
        end
      end
      return [] if minimum && maximum && minimum > maximum
      return [] if equal && (minimum && equal < minimum || maximum && equal > maximum)
      charge unless stored[:rows].empty?
      if equal
        row = stored[:native_primary_index][equal]
        row ? [row] : []
      else
        sorted = stored[:native_primary_sorted]
        first = minimum ? sorted.bsearch_index { |key, _| key >= minimum } || sorted.length : 0
        last = maximum ? sorted.bsearch_index { |key, _| key > maximum } || sorted.length : sorted.length
        sorted[first...last].map(&:last)
      end
    end

    def select(statement)
      return super unless cacheable_select?(statement)
      source = statement[:from]&.first
      stored = source && table(source[:name])
      types = @native_binding_types || @params.map { |value| Types.infer(value) }
      context = [@types, @environment, timezone, default_collation]
      @native_select_plans ||= {}.compare_by_identity
      plan = @native_select_plans[statement]
      unless plan && plan.table.equal?(stored) && plan.parameter_types == types && plan.context == context
        plan = resolve_select_plan(statement, source, stored, types)
        plan.context = context
        @native_select_plans.shift if @native_select_plans.length >= CACHE_ENTRIES
        @native_select_plans[statement] = plan
      end
      plan.parameters.each do |parameter|
        cast_value(@params[parameter.value], parameter.type) if parameter.extra&.dig(:context_type)
      end
      pipeline_mode = :array if stored && !plan.lookup && plan.orders.empty? &&
                                !cursor_output? && !@query_memory && !@explain_profile_frames && !@native_pipeline_disabled
      kernels = native_compiled_kernels(plan, plan.predicate, plan.projections, pipeline: pipeline_mode)
      pipeline = kernels&.pipeline if pipeline_mode
      predicate_kernel, projection_kernel = kernels&.predicate, kernels&.projection
      input = if where_contradiction?(plan.predicate)
        []
      elsif plan.lookup
        key = evaluate(plan.lookup, [])
        if !key.nil? && stored[:native_primary_ordinal]
          key_type = stored[:columns][stored[:native_primary_ordinal]][:type]
          normalized = normalize_scan_bound(key_type, key)
          key = normalized == :unmatchable ? nil : normalized
          bits = Types::INTEGER_BITS[key_type]
          key = nil if key && bits && !key.between?(-(1 << (bits - 1)), (1 << (bits - 1)) - 1)
        end
        charge(stored.dig(:native_primary_metadata, :height) || 1) unless key.nil? || stored[:rows].empty?
        row = key.nil? ? nil : plan.index[key]
        if row.is_a?(PagedRow)
          charge_scan_payload(stored, [row])
        elsif row && stored[:native_row_positions]
          charge_scan_payload(stored, [stored[:native_row_positions][row.object_id]])
        end
        row ? [row] : []
      elsif stored
        candidates = primary_scan(stored, plan.predicate)
        unless candidates
          charge(full_scan_pages(stored))
          charge_scan_payload(stored)
        end
        candidates || (stored[:native_primary_entries] ? stored[:native_primary_entries].map { |_, ordinal| stored[:rows][ordinal] } : stored[:rows])
      else
        [[]]
      end
      charge(input.length) if stored
      if @query_memory
        gathered_owner = query_memory_owner
        output_owner = query_memory_owner if query_memory_collect_result?
        if !stored
          query_memory_add(gathered_owner, MemorySchedule::ROW) unless input.empty? || plan.predicate&.kind == :literal && plan.predicate.value != true
        end
      end
      if stored && !plan.predicate && plan.projections.all? { |expression| expression.kind == :column } &&
         (@query_memory || columnar_cursor?(statement))
        if @max_cost.positive? || @lifetime_max_cost.positive? || @explain_profile_frames
          # Preserve the eager metered/profile lane's original incremental
          # reservations, including rejection before a later storage read.
          gathered = plan.projections.map do |expression|
            input.map do |row|
              value = row[expression.value]
              query_memory_add(gathered_owner, LogicalMemory.value_bytes(value, expression.type))
              value
            end
          end
        else
          ordinals = plan.projections.map(&:value).uniq.sort
          lanes = ordinals.to_h { |ordinal| [ordinal, []] }
          input.each do |row|
            ordinals.each { |ordinal| lanes[ordinal] << row[ordinal] }
          end
          if gathered_owner
            bytes = ordinals.sum do |ordinal|
              type = stored[:columns][ordinal][:type]
              lanes[ordinal].sum { |value| LogicalMemory.value_bytes(value, type) }
            end
            query_memory_add(gathered_owner, bytes)
          end
          gathered = plan.projections.map { |expression| lanes[expression.value] }
        end
      end
      profile_checkpoint(:scan) if @explain_profile_frames
      streaming = cursor_output?
      output, sort_keys = cursor_output(plan.columns), []
      if pipeline
        pipeline.call(self, input, @params, output)
      else
        row_index = 0
        while row_index < input.length
          row = input[row_index]
          row_index += 1
          filter_start = @cost if @explain_profile_frames
          accepted = !plan.predicate || (predicate_kernel ? predicate_kernel.call(self, row, @params) : evaluate(plan.predicate, row)) == true
          profile_accumulate(:filter, @cost - filter_start) if @explain_profile_frames
          next unless accepted
          if projection_kernel && !gathered
            values = projection_kernel.call(self, row, @params)
          else
            values = Array.new(plan.projections.length)
            column_index = 0
            while column_index < values.length
              expression = plan.projections[column_index]
              values[column_index] = if gathered
                # Gathered values are stored cells, not evaluated columns.
                # Escaping text must receive the declared column's label just
                # as ordinary evaluation does, without rereading the storage.
                CollationRules.wrap(gathered[column_index][row_index - 1], expression.instance_variable_get(:@native_collation))
              else
                evaluate(expression, row)
              end
              column_index += 1
            end
          end
          query_memory_add_row(output_owner, values, plan.columns) if output_owner
          charge if streaming
          output << values
          unless plan.orders.empty?
            sort_start = @cost if @explain_profile_frames
            sort_keys << plan.orders.map { |kind, expression, _| kind == :output ? values[expression] : evaluate(expression, row) }
            profile_accumulate(:sort, @cost - sort_start) if @explain_profile_frames
          end
        end
      end
      if @explain_profile_frames
        profile_inherit(:filter, :scan)
        profile_inherit(:sort, plan.predicate ? :filter : :scan)
      end
      unless plan.orders.empty?
        indices = (0...output.length).to_a
        indices.sort! do |left, right|
          compare = 0
          index = 0
          while index < plan.orders.length
            compare = order_compare(sort_keys[left][index], sort_keys[right][index], plan.orders[index][2])
            break unless compare.zero?
            index += 1
          end
          compare
        end
        output = indices.map { |index| output[index] }
      end
      charge(output.length) unless streaming
      profile_checkpoint(:root) if @explain_profile_frames
      result = Relation.new(columns: plan.columns, rows: output)
      query_memory_attach(result, output_owner) if output_owner
      result
    ensure
      query_memory_release(gathered_owner) if gathered_owner && @query_memory
    end

    private

    # Streaming and sorted readers share resolution. General predicates choose
    # access paths for each binding; an unambiguous exact point retains its path.
    # No subquery, outer scope, aggregate or clock-dependent expression is retained.
    def resolved_read_plan(statement, stored)
      reusable = @outer.empty? && @ctes.empty?
      context = [@types, @environment, timezone, default_collation, @native_binding_types]
      @native_read_plans ||= {}.compare_by_identity
      plan = reusable && @native_read_plans[statement]
      unless plan && plan.table.equal?(stored) && plan.context == context
        nodes = statement[:columns].map { |entry| entry[:expr] } + [statement[:where]] +
                (statement[:order_by] || []).map { |entry| entry[:expr] }
        reusable &&= nodes.all? { |node| cacheable_read_expression?(node) }
        source = statement[:from].first
        scope = table_columns(stored, source[:alias] || source[:name])
        projections = expand_stars(statement[:columns], scope)
        expressions = projections.map { |entry| compile(entry[:expr], scope) }
        columns = projections.zip(expressions).map { |entry, expression| { name: entry[:alias] || expression_name(entry[:expr]), type: expression.type } }
        predicate = compile_filter(statement[:where], scope)
        parameters = []
        visit = lambda do |expression|
          if expression.is_a?(Array)
            expression.each { |child| visit.call(child) }
            next
          end
          return unless expression.is_a?(Expression)
          parameters << expression if expression.kind == :param && expression.extra&.dig(:context_type)
          (expression.args || []).each { |child| visit.call(child) }
        end
        (expressions + [predicate]).each { |expression| visit.call(expression) }
        plan = ReadPlan.new(table: stored, scope: scope, projections: projections, expressions: expressions,
                            columns: columns, predicate: predicate, parameters: parameters, context: context,
                            scan_columns: scan_column_inventory(statement))
        plan.point = prepared_stream_point(statement, stored, predicate) if reusable
        if reusable
          @native_read_plans.shift if @native_read_plans.length >= CACHE_ENTRIES
          @native_read_plans[statement] = plan
        end
      end
      plan.parameters.each { |parameter| cast_value(@params[parameter.value], parameter.type) }
      plan
    end

    # Read-plan reuse does not choose a different executor lane. It can retain
    # conditional expression trees while the eager SELECT eligibility above
    # keeps its original charge and error ordering. Inspect every branch even
    # when a particular binding would not execute it.
    def cacheable_read_expression?(node)
      return true unless node
      case node[:kind]
      when :case
        cacheable_read_expression?(node[:operand]) &&
          node[:branches].all? do |branch|
            condition, value = branch.is_a?(Array) ? branch : [branch[:when], branch[:then]]
            cacheable_read_expression?(condition) && cacheable_read_expression?(value)
          end && cacheable_read_expression?(node[:else])
      when :between
        cacheable_read_expression?(node[:expr]) &&
          cacheable_read_expression?(node[:low] || node[:lower]) &&
          cacheable_read_expression?(node[:high] || node[:upper])
      when :cast, :unary
        cacheable_read_expression?(node[:expr] || node[:operand])
      when :binary
        cacheable_read_expression?(node[:left]) && cacheable_read_expression?(node[:right])
      when :call
        (node[:args] || []).all? { |argument| cacheable_read_expression?(argument) } &&
          cacheable_expression?(node.merge(args: []), host: true)
      else
        cacheable_expression?(node, host: true)
      end
    end

    # A single exact integer key has only a full scan and equivalent primary
    # bounds when no other access candidate applies. The primary bound always
    # wins that comparison, independent of the binding or statistics. Resolve
    # this once beside the expressions, then use the existing point index on
    # each pull. More involved predicates still use the ordinary cost planner.
    def prepared_stream_point(statement, stored, predicate)
      return unless !(statement[:distinct] || statement[:limit] || statement[:offset]) && (statement[:order_by] || []).empty?
      return unless stored[:native_primary_index] && stored[:collation_skew].to_a.empty?
      return unless predicate&.kind == :binary && predicate.value == '='
      left, right = predicate.args
      left, right = right, left if right.kind == :column
      return unless left.kind == :column && left.value == stored[:native_primary_ordinal]
      return unless %i[literal param].include?(right.kind) && Types::INTEGER_BITS.key?(right.type) && !right.extra&.dig(:subquery_origin)
      candidates = scan_access_candidates(stored, predicate)
      return unless candidates.all? { |candidate| %i[full primary primary_interval].include?(candidate[:kind]) }
      bits = Types::INTEGER_BITS.fetch(stored[:columns][stored[:native_primary_ordinal]][:type])
      PointRead.new(expression: right, index: stored[:native_primary_index],
        minimum: -(1 << (bits - 1)), maximum: (1 << (bits - 1)) - 1,
        pages: stored[:rows].empty? ? 0 : stored.dig(:native_primary_metadata, :height) || 1)
    end

    # A bare-column cursor retains the ordinary projection lane's gathered
    # state, independently of the memory limit. Execution starts on its first
    # pull and each output is yielded separately. Expressions, filters and
    # windowed scans keep their existing lazy lanes.
    def columnar_cursor?(statement)
      return false unless cursor_output? && statement[:kind] == :select
      # Cost ceilings and profiling keep the row lane's incremental checkpoints.
      return false if @max_cost.positive? || @lifetime_max_cost.positive? || @explain_profile_frames
      return false unless @outer.empty? && @ctes.empty?
      return false if statement[:where] || statement[:with] || statement[:distinct] || statement[:distinct_on] ||
                      statement[:having] || statement[:limit] || statement[:offset]
      return false unless (statement[:joins] || []).empty? && (statement[:group_by] || []).empty? &&
                          (statement[:windows] || {}).empty? && (statement[:order_by] || []).empty?
      sources = statement[:from] || []
      return false unless sources.length == 1
      source = sources.first
      return false unless source[:kind] == :table && @tables.key?(source[:name]) &&
                          !source[:database] && !source[:columns] && !source[:column_aliases]
      statement[:columns].all? { |projection| %i[column star].include?(projection[:expr][:kind]) }
    end

    def cacheable_select?(statement)
      return false if @query_memory && !(statement[:order_by] || []).empty?
      if @query_memory && query_memory_collect_result? && statement[:from]&.any?
        return false if statement[:where] || !statement[:columns].all? { |projection| %i[column star].include?(projection[:expr][:kind]) }
      end
      return false unless @outer.empty? && @ctes.empty?
      return false unless statement[:kind] == :select
      return false if statement[:with] || statement[:distinct] || statement[:distinct_on] || statement[:having] || statement[:limit] || statement[:offset]
      return false unless (statement[:joins] || []).empty? && (statement[:group_by] || []).empty? && (statement[:windows] || {}).empty?
      sources = statement[:from] || []
      return false if sources.length > 1
      if (source = sources.first)
        return false unless source[:kind] == :table && @tables.key?(source[:name]) && !source[:database] && !source[:columns] && !source[:column_aliases]
        return false if @ctes.key?(source[:name])
      end
      statements = statement[:columns].map { |c| c[:expr] } + [statement[:where]] + (statement[:order_by] || []).map { |o| o[:expr] }
      statements.all? { |expression| cacheable_expression?(expression) }
    end

    def cacheable_expression?(node, host: false)
      return true unless node
      case node[:kind]
      when :literal, :column, :param, :star then true
      when :cast, :unary then cacheable_expression?(node[:expr], host: host)
      when :binary then cacheable_expression?(node[:left], host: host) && cacheable_expression?(node[:right], host: host)
      when :call
        !STATEFUL_FUNCTIONS.include?(node[:name]) && !node[:over] && !node[:filter] && !node[:within_group] && !Functions.aggregate?(node[:name]) && (node[:args] || []).all? { |arg| cacheable_expression?(arg, host: host) }
      else false
      end
    end

    def resolve_select_plan(statement, source, stored, types)
      columns = stored ? table_columns(stored, source[:alias] || source[:name]) : []
      projections = expand_stars(statement[:columns], columns)
      expressions = projections.map { |p| compile(p[:expr], columns) }
      outputs = projections.zip(expressions).map { |p, e| { name: p[:alias] || expression_name(p[:expr]), type: e.type } }
      predicate = compile_filter(statement[:where], columns)
      orders = (statement[:order_by] || []).map do |order|
        node = order[:expr]
        if node[:kind] == :literal && node[:value].is_a?(Integer)
          index = node[:value] - 1
          raise Error.new('42P10', 'ORDER BY position out of range') unless index.between?(0, outputs.length - 1)
          [:output, index, order]
        elsif node[:kind] == :column && !node[:table] && (index = outputs.index { |c| c[:name] == node[:name] })
          [:output, index, order]
        else
          [:input, compile(node, columns), order]
        end
      end
      orders.each { |kind, expression, _| ensure_comparable_type(kind == :output ? outputs[expression][:type] : expression.type) }
      parameters = []
      visit = lambda do |expression|
        if expression.is_a?(Array)
          expression.each { |child| visit.call(child) }
          next
        end
        return unless expression.is_a?(Expression)
        parameters << expression if expression.kind == :param
        (expression.args || []).each { |arg| visit.call(arg) }
      end
      (expressions + [predicate] + orders.filter_map { |kind, e, _| e if kind == :input }).each { |e| visit.call(e) }
      lookup, index = nil, nil
      if stored && predicate&.kind == :binary && predicate.value == '='
        if (ordinal = stored[:native_primary_ordinal])
          left, right = predicate.args
          left, right = right, left if right.kind == :column
          if left.kind == :column && left.value == ordinal && %i[literal param].include?(right.kind) && Types::INTEGER_BITS.key?(right.type) && !right.extra&.dig(:subquery_origin)
            lookup = right
            index = stored[:native_primary_index]
          end
        end
      end
      Plan.new(table: stored, source: source, columns: outputs, projections: expressions,
               predicate: predicate, orders: orders, parameters: parameters,
               parameter_types: types, lookup: lookup, index: index)
    end
  end
  Engine.prepend(PreparedSelect)
end
