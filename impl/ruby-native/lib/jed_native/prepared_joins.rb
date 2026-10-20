# frozen_string_literal: true

module JedNative
  # Retain resolution, not a parameter's winning physical plan. Rewriting,
  # estimates, access selection and all row operators still run on every call.
  module PreparedJoins
    CACHE_ENTRIES = 64
    ROOT_ENTRIES = 128
    KEY_UNITS = 4096
    KEY_BYTES = 65_536
    SCALAR_TYPES = %w[i16 i32 i64 boolean text].freeze
    BINARY = %w[+ - * / % & | # << >> = <> != < <= > >= and or is\ distinct\ from is\ not\ distinct\ from].freeze
    UNARY = %w[+ - ~ not is\ null is\ not\ null is\ true is\ false is\ unknown is\ not\ true is\ not\ false is\ not\ unknown].freeze
    Entry = Struct.new(:tables, :context, :roots, :columns, :key_units, :key_bytes, keyword_init: true)
    Frame = Struct.new(:entry, :depth, :checks, :plans, keyword_init: true)

    def select(statement)
      previous = @native_join_resolution_frame
      return super unless previous || statement[:joins]&.length == 1
      wrapped = true
      @native_join_resolution_frame = prepared_join_frame(statement)
      super
    ensure
      @native_join_resolution_frame = previous if wrapped
    end

    def compile(node, columns)
      frame = @native_join_resolution_frame
      return super unless frame && frame.depth.zero?
      prepared_join_root(frame, :value, node, columns) { super }
    end

    def compile_filter(node, columns)
      frame = @native_join_resolution_frame
      return super unless frame && frame.depth.zero? && node
      # Filter AND and value AND have different NULL short-circuit rules.
      prepared_join_root(frame, :filter, node, columns) { super }
    end

    def contextual_binary(operator, left, right)
      frame = @native_join_resolution_frame
      checks = frame&.checks
      if checks && !%w[-> ->> #> #>> ? ?| ?& ||].include?(operator)
        comparison = Resolution::COMPARISONS.include?(operator)
        numeric = %w[i16 i32 i64 f32 f64 decimal]
        # Children finish before their parent's contextual checks. Reuse runs
        # the same checks at this compile call against the current binding.
        [[left, right], [right, left]].each do |constant, other|
          next unless constant.kind == :param && !%i[literal param].include?(other.kind)
          next unless comparison || numeric.include?(constant.type) && numeric.include?(other.type)
          checks << [constant.value, other.type].freeze
        end
      end
      super
    end

    def table_columns(stored, name = stored[:name])
      frame = @native_join_resolution_frame
      return super unless frame && frame.entry.tables.any? { |_, table| table.equal?(stored) }
      columns = frame.entry.columns
      key = [stored.object_id, name]
      return columns[key] if columns.key?(key)
      columns[[stored.object_id, name.dup.freeze].freeze] = super.map(&:freeze).freeze
    end

    def two_base_join_plan(first, edge, where, statement)
      frame = @native_join_resolution_frame
      return super unless frame
      # File spill preflight and materialization ask for the same custom plan.
      # This memo belongs to one SELECT invocation, never to the shared entry.
      key = [first, edge, where, statement]
      frame.plans[key] ||= super
    end

    def refresh_primary_indexes
      super
      @native_join_resolution_plans&.delete_if do |_, entry|
        entry.tables.any? { |name, stored| !@tables[name].equal?(stored) }
      end
    end

    private

    def prepared_join_frame(statement)
      return if @native_join_resolution_disabled || !@outer.empty? || !@ctes.empty? ||
                @explain_runtime_plans || @explain_profile_frames || @native_explain_contradiction
      return unless statement[:kind] == :select && !statement[:with] && !statement[:distinct] &&
                    !statement[:distinct_on] && !statement[:having] && (statement[:group_by] || []).empty? &&
                    (statement[:windows] || []).empty?
      sources, joins = statement[:from] || [], statement[:joins] || []
      return unless sources.length == 1 && joins.length == 1
      edge = joins.first
      return unless %i[inner cross left left_outer].include?(edge[:kind]) && !edge[:using] && !edge[:natural]
      inputs = [sources.first, edge[:relation]]
      return unless inputs.all? do |source|
        source && source[:kind] == :table && !source[:lateral] && !source[:columns] && !source[:column_aliases] &&
          @tables.key?(source[:name]) && !@views.key?(source[:name])
      end
      tables = inputs.map { |source| [source[:name], @tables.fetch(source[:name])] }
      return unless tables.sum { |_, table| table[:columns].length } <= 64 && tables.all? do |_, table|
        table[:columns].all? { |column| SCALAR_TYPES.include?(column[:type]) && !@types.key?(column[:type]) }
      end
      types = @native_binding_types || []
      return unless types.all? { |type| SCALAR_TYPES.include?(type) && !@types.key?(type) }
      context = [@types, @environment, timezone, default_collation, types]
      @native_join_resolution_plans ||= {}.compare_by_identity
      entry = @native_join_resolution_plans[statement]
      unless entry && entry.context == context && entry.tables.zip(tables).all? { |old, current| old[0] == current[0] && old[1].equal?(current[1]) }
        return unless prepared_join_syntax?(statement)
        # Alias/table names and column names also live in retained scopes, even
        # when the original expression is only '*'. Bound them before copying.
        names = inputs.flat_map { |source| [source[:name], source[:alias]] } +
          tables.flat_map { |_, table| table[:columns].map { |column| column[:name] } }
        return if names.compact.sum(&:bytesize) > 32_768
        entry = Entry.new(tables: tables.freeze, context: context.freeze, roots: {}, columns: {}, key_units: 0, key_bytes: 0)
        @native_join_resolution_plans.shift if @native_join_resolution_plans.length >= CACHE_ENTRIES
        @native_join_resolution_plans[statement] = entry
      end
      Frame.new(entry: entry, depth: 0, plans: {})
    end

    def prepared_join_syntax?(statement)
      return false if statement[:columns].length + (statement[:order_by] || []).length > 192
      roots = statement[:columns].map { |item| item[:expr] } + [statement[:where], statement[:joins].first[:on],
        statement[:limit], statement[:offset]] + (statement[:order_by] || []).map { |order| order[:expr] }
      prepared_join_nodes?(roots)
    end

    def prepared_join_nodes?(roots)
      pending = roots.compact.map { |node| [node, 0] }
      nodes, bytes = 0, 0
      until pending.empty?
        node, depth = pending.pop
        nodes += 1
        return false if nodes > 192 || depth > 32
        return false unless node.is_a?(Hash)
        node.each_value { |value| bytes += value.bytesize if value.is_a?(String) }
        return false if bytes > 32_768
        children = case node[:kind]
                   when :column, :param, :star then []
                   when :literal
                     value = node[:value]
                     return false unless SCALAR_TYPES.include?(node[:type]) || %w[unknown null].include?(node[:type])
                     return false unless value.nil? || value == true || value == false || value.is_a?(Integer) || value.is_a?(String)
                     []
                   when :cast
                     return false unless Types::INTEGER_BITS.key?(node[:type]) && !node[:type_mod]
                     child = node[:expr]
                     return false if child[:kind] == :literal && child[:value].is_a?(String)
                     [child]
                   when :unary
                     return false unless UNARY.include?(node[:op])
                     [node[:expr] || node[:operand]]
                   when :binary
                     return false unless BINARY.include?(node[:op])
                     [node[:left], node[:right]]
                   else return false
                   end
        children.each { |child| pending << [child, depth + 1] }
      end
      true
    end

    def prepared_join_root(frame, mode, node, columns)
      key = [mode, node, columns]
      if (saved = frame.entry.roots[key])
        saved[1].each { |index, type| cast_value(@params[index], type) }
        return saved[0]
      end
      before = @cost
      frame.depth += 1
      frame.checks = []
      expression = yield
      # Only successful, unmetered resolution is retained. Keys own snapshots
      # because planner rewrites manufacture fresh equivalent AST/scope hashes.
      if @cost == before && frame.entry.roots.length < ROOT_ENTRIES && prepared_join_nodes?([node]) &&
         (size = prepared_join_key_size(key, frame.entry))
        snapshot = prepared_join_key(key)
        frame.entry.roots[snapshot] = [expression, frame.checks.freeze].freeze
        frame.entry.key_units += size[0]
        frame.entry.key_bytes += size[1]
      end
      expression
    ensure
      if before
        frame.depth -= 1
        frame.checks = nil
      end
    end

    def prepared_join_key_size(value, entry)
      units, bytes, pending = 0, 0, [value]
      until pending.empty?
        item = pending.pop
        units += 1
        case item
        when Array then pending.concat(item)
        when Hash then item.each { |key, child| pending << key << child }
        when String then bytes += item.bytesize
        end
        return if entry.key_units + units > KEY_UNITS || entry.key_bytes + bytes > KEY_BYTES
      end
      [units, bytes]
    end

    def prepared_join_key(value)
      case value
      when Array then value.map { |item| prepared_join_key(item) }.freeze
      when Hash then value.to_h { |key, item| [key, prepared_join_key(item)] }.freeze
      when String then value.dup.freeze
      else value
      end
    end
  end

  Engine.prepend(PreparedJoins)
end
