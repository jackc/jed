# frozen_string_literal: true

module JedNative
  # Retain bounded resolution and value-blind physical winners. Rewriting and
  # all row operators remain fresh; value-sensitive selection is never reused.
  module PreparedJoins
    CACHE_ENTRIES = 64
    ROOT_ENTRIES = 128
    KEY_UNITS = 4096
    KEY_BYTES = 65_536
    MAX_INPUTS = 8
    EXTENSION_ENTRIES = 128
    PHYSICAL_UNITS = 16_384
    PHYSICAL_BYTES = 131_072
    PHYSICAL_CHECKS = 4096
    SCALAR_TYPES = %w[i16 i32 i64 boolean text].freeze
    BINARY = %w[+ - * / % & | # << >> = <> != < <= > >= and or is\ distinct\ from is\ not\ distinct\ from].freeze
    UNARY = %w[+ - ~ not is\ null is\ not\ null is\ true is\ false is\ unknown is\ not\ true is\ not\ false is\ not\ unknown].freeze
    # These calls resolve to ordinary scalar expressions. Their evaluation,
    # collation and resource charging still belong to the current execution.
    # In particular, omit regex/format programs, clocks, aggregates and builders
    # with additional resolved state rather than admitting arbitrary calls.
    SCALAR_FUNCTIONS = %w[
      abs mod gcd lcm lower upper initcap length char_length character_length
      octet_length bit_length ascii chr reverse left right substr strpos
      starts_with btrim ltrim rtrim replace translate split_part
    ].freeze
    Entry = Struct.new(:tables, :context, :roots, :columns, :scopes, :nodes, :key_units, :key_bytes, :physical,
      :sources, :source_units, :source_bytes, keyword_init: true)
    Frame = Struct.new(:entry, :depth, :checks, :plans, :extension_context, :extensions, :physical_observation,
      :physical_reuse, keyword_init: true)
    Physical = Struct.new(:key, :plan, :checks, :units, :bytes, keyword_init: true)

    def select(statement)
      previous = @native_join_resolution_frame
      return super unless previous || !(statement[:joins] || []).empty? || (statement[:from] || []).length > 2
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
      observation = frame&.physical_observation
      if (checks || observation) && !%w[-> ->> #> #>> ? ?| ?& ||].include?(operator)
        comparison = Resolution::COMPARISONS.include?(operator)
        numeric = %w[i16 i32 i64 f32 f64 decimal]
        # Children finish before their parent's contextual checks. Reuse runs
        # the same checks at this compile call against the current binding.
        [[left, right], [right, left]].each do |constant, other|
          next unless constant.kind == :param && !%i[literal param].include?(other.kind)
          next unless comparison || numeric.include?(constant.type) && numeric.include?(other.type)
          next unless contextual_parameter_cast?(constant, other.type)
          check = [constant.value, other.type].freeze
          checks << check if checks
          prepared_join_physical_check(observation, check) if observation
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
      result = super
      return result if columns.length >= MAX_INPUTS
      # Public AST aliases can change between calls. Own their descriptor
      # strings, and share the existing source-metadata allowance so alias
      # churn cannot grow this map outside the bounded statement entry.
      snapshot = prepared_join_source_snapshot([key, result], frame.entry)
      return result unless snapshot
      owned, units, bytes = snapshot
      frame.entry.source_units += units
      frame.entry.source_bytes += bytes
      columns[owned[0]] = owned[1]
    end

    def two_base_join_plan(first, edge, where, statement)
      frame = @native_join_resolution_frame
      return super unless frame
      # File spill preflight and materialization ask for the same custom plan.
      # This memo belongs to one SELECT invocation, never to the shared entry.
      key = [first, edge, where, statement]
      frame.plans[key] ||= prepared_join_physical_plan(frame, [:two, *key]) { super }
    end

    def nway_join_plan(sources, joins, where, statement)
      frame = @native_join_resolution_frame
      return super unless frame
      prepared_join_physical_plan(frame, [:multi, sources, joins, where, statement]) { super }
    end

    def refresh_primary_indexes
      super
      @native_join_resolution_plans&.delete_if do |_, entry|
        entry.tables.any? { |name, stored| !@tables[name].equal?(stored) }
      end
    end

    private

    def query_source_columns(source, ctes, outer, analyzer)
      frame = @native_join_resolution_frame
      return super unless frame && !@native_query_walks_disabled && source[:kind] == :table &&
        !source[:database] && !source[:native_database] && !source[:lateral] && !source[:columns] && !source[:column_aliases] &&
        !ctes.key?(source[:name]) && !@views.key?(source[:name])
      entry = frame.entry
      name = source[:name]
      return super if name.start_with?('jed_')
      return super unless entry.tables.any? { |stored_name, stored| stored_name == name && @tables[name].equal?(stored) }
      key = [name, source[:alias]]
      return entry.sources[key] if entry.sources.key?(key)
      columns = super
      return columns if entry.sources.length >= MAX_INPUTS
      snapshot = prepared_join_source_snapshot([key, columns], entry)
      return columns unless snapshot
      owned, units, bytes = snapshot
      entry.source_units += units
      entry.source_bytes += bytes
      entry.sources[owned[0]] = owned[1]
    end

    def prepared_join_source_snapshot(value, entry)
      units, bytes = 0, 0
      copy = lambda do |item, depth|
        units += 1
        throw :unsupported if depth > 16 || entry.source_units + units > KEY_UNITS
        case item
        when Hash
          throw :unsupported unless item.instance_of?(Hash)
          item.to_h { |key, child| [copy.call(key, depth + 1), copy.call(child, depth + 1)] }.freeze
        when Array
          throw :unsupported unless item.instance_of?(Array)
          item.map { |child| copy.call(child, depth + 1) }.freeze
        when String
          throw :unsupported unless item.instance_of?(String)
          bytes += item.bytesize
          throw :unsupported if entry.source_bytes + bytes > KEY_BYTES
          item.dup.freeze
        when Symbol, Integer, NilClass, TrueClass, FalseClass
          item
        else
          throw :unsupported
        end
      end
      catch(:unsupported) { [copy.call(value, 0), units, bytes] }
    end

    def plan_constant(expression)
      observation = @native_join_resolution_frame&.physical_observation
      observation[:sensitive] = true if observation && expression&.kind == :param
      super
    end

    def plan_constant_null?(expression)
      # A NULL test cannot distinguish non-NULL bindings. Every NULL binding
      # takes fresh selection, so only a full value read prevents retention.
      if expression&.kind == :param
        return expression.value < @params.length && @params[expression.value].nil?
      end
      super
    end

    def contradiction_literal(expression)
      # Current candidate search leaves contradiction pruning outside this
      # frame. Fail closed if a future planner starts invoking it here.
      observation = @native_join_resolution_frame&.physical_observation
      observation[:sensitive] = true if observation && expression.kind == :param
      super
    end

    def prepared_join_physical_plan(frame, key)
      return yield unless frame.physical_reuse
      return yield if @native_join_physical_disabled || !@native_binding_types || @params.any?(&:nil?) ||
                      transaction_open? || @native_cursor&.block_bound? || frame.entry.tables.any? { |_, table| table[:temporary] }
      saved = frame.entry.physical
      if saved && saved.key == key
        saved.checks.each { |index, type| cast_value(@params[index], type) }
        return saved.plan
      end
      previous = frame.physical_observation
      observation = { sensitive: false, checks: [] }
      frame.physical_observation = observation
      before = @cost
      plan = yield
      if plan && frame.physical_reuse && @cost == before && !observation[:sensitive]
        # Normalized integer/boolean casts are pure and unmetered. Keep the
        # first of each duplicate, but preserve text conversions in order:
        # even an exact String can have a host-defined singleton to_s method.
        seen = {}
        checks = observation[:checks].reject do |check|
          source, target = @native_binding_types[check[0]], check[1]
          next false unless (source == 'boolean' || Types::INTEGER_BITS.key?(source)) &&
                            (target == 'boolean' || Types::INTEGER_BITS.key?(target))
          duplicate = seen.key?(check)
          seen[check] = true
          duplicate
        end
        retained = prepared_join_physical_snapshot([key, plan, checks], frame.entry)
        if retained
          values, units, bytes = retained
          frame.entry.physical = Physical.new(key: values[0], plan: values[1], checks: values[2], units: units, bytes: bytes).freeze
        end
      end
      plan
    ensure
      frame.physical_observation = previous if observation
    end

    def prepared_join_physical_check(observation, check)
      if observation[:checks].length < PHYSICAL_CHECKS
        observation[:checks] << check
      else
        observation[:sensitive] = true
      end
    end

    def prepared_join_physical_snapshot(value, entry)
      # Only plan-owned containers are copied/frozen. Table descriptors keep
      # their snapshot identity; resolved Expressions have symbolic parameters
      # and may acquire compiled kernels. No execution rows are retained.
      borrowed = entry.tables.to_h { |_, table| [table.object_id, true] }
      seen, units, bytes = {}, 0, 0
      copy = lambda do |item|
        units += 1
        throw :oversized if units > PHYSICAL_UNITS
        return item if borrowed.key?(item.object_id)
        return seen[item.object_id] if seen.key?(item.object_id)
        result = case item
                 when Hash
                   item.to_h { |key, child| [copy.call(key), copy.call(child)] }.freeze
                 when Array
                   item.map { |child| copy.call(child) }.freeze
                 when String
                   bytes += item.bytesize
                   throw :oversized if bytes > PHYSICAL_BYTES
                   item.dup.freeze
                 when Expression
                   # Account for retained expression metadata without copying
                   # or freezing its existing compilation state.
                   item.each_pair { |_, child| copy.call(child) }
                   item
                 when Symbol, Integer, NilClass, TrueClass, FalseClass
                   item
                 else
                   throw :oversized
                 end
        seen[item.object_id] = result
      end
      catch(:oversized) { [copy.call(value), units, bytes] }
    end

    def nway_extension_inputs(context, state, ordinal, fixed:)
      frame = @native_join_resolution_frame
      return super unless frame
      # Alternative accesses can reach the same ordered prefix within this
      # search. Only its scopes/predicates/index candidates are shared; cardinality,
      # step cost and mutable access hashes are still computed for each state.
      unless frame.extension_context.equal?(context)
        frame.extension_context = context
        frame.extensions = {}
      end
      key = [state[:order], ordinal, fixed]
      return frame.extensions[key] if frame.extensions.key?(key)
      before = @cost
      inputs = super
      frame.extensions[key] = inputs if @cost == before && frame.extensions.length < EXTENSION_ENTRIES
      inputs
    end

    def prepared_join_frame(statement)
      return if @native_join_resolution_disabled || !@outer.empty? || !@ctes.empty? ||
                @explain_runtime_plans || @explain_profile_frames || @native_explain_contradiction
      return unless statement[:kind] == :select && !statement[:with] && !statement[:distinct] &&
                    !statement[:distinct_on] && !statement[:having] && (statement[:group_by] || []).empty? &&
                    (statement[:windows] || []).empty?
      sources, joins = statement[:from] || [], statement[:joins] || []
      inputs = sources + joins.map { |edge| edge[:relation] }
      return unless inputs.length.between?(2, MAX_INPUTS)
      return unless sources.length == 1 || sources.length > 2 && joins.empty?
      kinds = %i[inner cross left left_outer]
      return unless joins.all? { |edge| kinds.include?(edge[:kind]) && !edge[:using] && !edge[:natural] }
      # Resolution can be reused across a fixed outer edge, but the multiway
      # physical winner remains execution-specific. Check the current syntax
      # even when an entry already holds a winner from an earlier INNER shape.
      physical_reuse = inputs.length == 2 ||
        joins.none? { |edge| edge[:kind] == :left || edge[:kind] == :left_outer }
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
      unless entry && entry.context == context && entry.tables.length == tables.length &&
             entry.tables.zip(tables).all? { |old, current| old[0] == current[0] && old[1].equal?(current[1]) }
        return unless prepared_join_syntax?(statement)
        # Alias/table names and column names also live in retained scopes, even
        # when the original expression is only '*'. Bound them before copying.
        names = inputs.flat_map { |source| [source[:name], source[:alias]] } +
          tables.flat_map { |_, table| table[:columns].map { |column| column[:name] } }
        return if names.compact.sum(&:bytesize) > 32_768
        entry = Entry.new(tables: tables.freeze, context: context.freeze, roots: {}, columns: {}, scopes: {}, nodes: {},
          key_units: 0, key_bytes: 0, sources: {}, source_units: 0, source_bytes: 0)
        @native_join_resolution_plans.shift if @native_join_resolution_plans.length >= CACHE_ENTRIES
        @native_join_resolution_plans[statement] = entry
      end
      Frame.new(entry: entry, depth: 0, plans: {}, physical_reuse: physical_reuse)
    end

    def prepared_join_syntax?(statement)
      return false if statement[:columns].length + (statement[:order_by] || []).length > 192
      roots = statement[:columns].map { |item| item[:expr] } + [statement[:where], statement[:limit], statement[:offset]] +
        (statement[:joins] || []).map { |edge| edge[:on] } + (statement[:order_by] || []).map { |order| order[:expr] }
      prepared_join_nodes?(roots)
    end

    def prepared_join_nodes?(roots, expression = nil)
      pending = roots.compact.map { |node| [node, 0] }
      nodes, bytes = 0, 0
      calls = false
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
                   when :call
                     return false unless prepared_join_scalar_call?(node)
                     calls = true
                     node[:args]
                   else return false
                   end
        return false if nodes + pending.length + children.length > 192
        children.each { |child| pending << [child, depth + 1] }
      end
      !calls || !expression || prepared_join_scalar_tree?(expression)
    end

    def prepared_join_scalar_call?(node)
      SCALAR_FUNCTIONS.include?(node[:name]) && !host_function_name?(node[:name]) &&
        !node[:over] && !node[:filter] && !node[:distinct] && !node[:within_group] && !node[:variadic] &&
        (node[:order_by] || []).empty? && (node[:arg_names] || []).empty? && node[:args].instance_of?(Array)
    end

    def prepared_join_scalar_tree?(expression)
      # Validate only on publication, never on a warm root hit. Literal overload
      # resolution may rewrite arguments, so syntax admission alone is not the
      # ownership/type boundary of the retained resolved tree.
      pending = [[expression, 0]]
      nodes, bytes = 0, 0
      until pending.empty?
        item, depth = pending.pop
        nodes += 1
        return false if nodes > 192 || depth > 32
        return false unless item.instance_of?(Expression) &&
          (SCALAR_TYPES.include?(item.type) || %w[unknown null].include?(item.type)) && !@types.key?(item.type)
        case item.kind
        when :literal
          value = item.value
          return false unless value.nil? || value == true || value == false || value.is_a?(Integer) || value.is_a?(String)
          bytes += value.bytesize if value.is_a?(String)
          return false if bytes > 32_768
        when :column, :param, :star
        when :unary
          return false unless UNARY.include?(item.value)
        when :binary
          return false unless BINARY.include?(item.value)
        when :cast
          return false unless Types::INTEGER_BITS.key?(item.type) && !item.extra&.dig(:type_mod)
        when :call
          return false unless SCALAR_FUNCTIONS.include?(item.value) && !host_function_name?(item.value)
        else return false
        end
        children = item.args
        next unless children
        return false unless children.instance_of?(Array) && nodes + pending.length + children.length <= 192
        children.each { |child| pending << [child, depth + 1] }
      end
      true
    end

    def prepared_join_root(frame, mode, node, columns)
      # Physical prefixes revisit the same AST in different ordered scopes.
      # Own each AST/scope once within the existing key budget; a root's tokens
      # still identify the complete expression and exact column order.
      scope = frame.entry.scopes[columns]
      syntax = frame.entry.nodes[node]
      key = [mode, syntax || frame.entry.nodes.length, scope || frame.entry.scopes.length]
      if scope && syntax && (saved = frame.entry.roots[key])
        saved[1].each do |check|
          cast_value(@params[check[0]], check[1])
          prepared_join_physical_check(frame.physical_observation, check) if frame.physical_observation
        end
        return saved[0]
      end
      before = @cost
      frame.depth += 1
      frame.checks = []
      expression = yield
      admitted = prepared_join_nodes?([node], expression)
      unless admitted
        # An existing statement entry can see a changed AST or a planner-made
        # root. Never let unsupported syntax/resolution enter a physical winner
        # merely because its original statement once passed the entry gate.
        frame.physical_reuse = false
        frame.physical_observation[:sensitive] = true if frame.physical_observation
      end
      # Only successful, unmetered resolution is retained. Keys own snapshots
      # because planner rewrites manufacture fresh equivalent AST/scope hashes.
      if @cost == before && frame.entry.roots.length < ROOT_ENTRIES && admitted
        retained = [key]
        retained << columns unless scope
        retained << node unless syntax
        size = prepared_join_key_size(retained, frame.entry)
        return expression unless size
        unless scope
          scope = frame.entry.scopes.length
          frame.entry.scopes[prepared_join_key(columns)] = scope
        end
        frame.entry.nodes[prepared_join_key(node)] = frame.entry.nodes.length unless syntax
        frame.entry.roots[key.freeze] = [expression, frame.checks.freeze].freeze
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
