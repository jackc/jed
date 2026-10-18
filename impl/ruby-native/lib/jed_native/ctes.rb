# frozen_string_literal: true

module JedNative
  # A CTE keeps its declaration scope. Query bodies can be evaluated at their
  # reference sites; writable bodies always produce one materialized buffer.
  module CTEs
    CTEBinding = Struct.new(:columns, :rows, :query, :ctes, :outer, :side_effect, :native_body_pushable, keyword_init: true)

    def cte_body_pushable?(name)
      binding = @ctes[name]
      binding.is_a?(CTEBinding) && binding.native_body_pushable == true
    end

    def dispatch(statement)
      owns_structure = !@cte_structure_active
      if owns_structure
        validate_cte_structure(statement, true)
        @cte_structure_active = true
      end
      owns_pin = !@cte_read_tables && writable_with?(statement)
      if owns_pin
        @cte_read_tables = @tables.dup
        @cte_working_tables = @tables
      end
      super
    ensure
      if owns_pin
        @tables = @cte_working_tables
        @cte_read_tables = @cte_working_tables = nil
      end
      @cte_structure_active = false if owns_structure
    end

    def select(statement)
      previous_tables = @tables
      @tables = @cte_read_tables if @cte_read_tables
      super
    ensure
      @tables = previous_tables if @cte_read_tables
    end

    def table(name)
      if @cte_mutation && !@cte_mutation[:merged] && @cte_read_tables.key?(name)
        @cte_read_tables[name]
      else
        super
      end
    end

    def compile(node, columns)
      if node[:kind] == :field && node.dig(:expr, :kind) == :column
        source = node[:expr]
        column = (columns + @outer.reverse.flat_map(&:first)).find do |candidate|
          candidate[:name] == source[:name] && (!source[:table] || candidate[:table] == source[:table])
        end
        if (fields = column&.dig(:record_fields))
          index = fields.index { |field| field[:name] == node[:name] }
          raise Error.new('42703', 'record field does not exist') unless index
          return Expression.new(kind: :field, type: fields[index][:type], value: index, args: [compile(source, columns)])
        end
      end
      super
    end

    def cte_search_cycle_columns(cte, columns)
      names = columns.map { |column| column[:name] }
      added = []
      [cte[:search], cte[:cycle]].compact.each do |clause|
        keys = clause[:columns]
        raise Error.new('42701', 'column appears twice in SEARCH/CYCLE') unless keys.uniq.length == keys.length
        raise Error.new('42601', 'SEARCH/CYCLE column does not exist') unless (keys - names).empty?
      end
      fields_for = lambda do |keys|
        keys.each_with_index.map { |name, index| { name: "f#{index + 1}", type: columns.find { |column| column[:name] == name }[:type] } }
      end
      if (search = cte[:search])
        fields = fields_for.call(search[:columns])
        if search[:order] == 'breadth'
          fields = [{ name: 'f1', type: 'i64' }] + fields.each_with_index.map { |field, index| field.merge(name: "f#{index + 2}") }
        end
        added << { name: search[:column], type: search[:order] == 'depth' ? 'record[]' : 'record', record_fields: fields }
      end
      if (cycle = cte[:cycle])
        mark_type, = cycle_mark_values(cycle)
        added << { name: cycle[:mark], type: mark_type }
        added << { name: cycle[:path], type: 'record[]', record_fields: fields_for.call(cycle[:columns]) }
      end
      all_names = names + added.map { |column| column[:name] }
      raise Error.new('42601', 'SEARCH/CYCLE output column already exists') unless all_names.uniq.length == all_names.length
      added
    end

    def materialize_ctes(statement)
      items = statement[:with] || statement[:ctes] || []
      names = items.map { |item| item[:name] }
      raise Error.new('42712', 'duplicate CTE name') unless names.uniq.length == names.length
      primary = statement.reject { |key, _| %i[with ctes].include?(key) }
      items.each_with_index do |cte, ordinal|
        body = cte[:query]
        if statement[:recursive] && cte_reference_count(body, cte[:name]).positive?
          materialize_recursive_cte(cte)
          next
        end
        references = cte_reference_count(primary, cte[:name])
        read_from_mutation = %i[insert update delete].include?(primary[:kind]) && references.positive?
        items.drop(ordinal + 1).each do |later|
          count = cte_reference_count(later[:query], cte[:name])
          references += count
          read_from_mutation ||= %i[insert update delete].include?(later[:query][:kind]) && count.positive?
        end
        modifying = %i[insert update delete].include?(body[:kind])
        if modifying && !body[:returning] && references.positive?
          raise Error.new('0A000', 'data-modifying CTE has no RETURNING clause')
        end
        columns = query_metadata(body, @outer.flat_map(&:first))[:columns]
        if cte[:columns] && cte[:columns].length > columns.length
          raise Error.new('42P10', 'too many CTE column names')
        end
        columns = columns.each_with_index.map { |column, index| column.merge(name: cte[:columns]&.[](index) || column[:name]) }
        declaration = @ctes.dup
        materialized = modifying || references.positive? && (cte[:materialized] == true || cte[:materialized].nil? && references > 1)
        if materialized
          definition_start = @cost
          if modifying
            result = dispatch(body)
            relation = Relation.new(columns: columns, rows: result.rows)
          else
            relation = select(body)
          end
          @ctes[cte[:name]] = CTEBinding.new(columns: columns, rows: relation.rows, side_effect: modifying && !body[:returning])
          explain_cte_definition(cte[:name], definition_start) if respond_to?(:explain_cte_definition)
        else
          @ctes[cte[:name]] = CTEBinding.new(columns: columns, query: body, ctes: declaration, outer: @outer.dup,
                                           native_body_pushable: references == 1 && !read_from_mutation && !@cte_mutation)
        end
      end
    end

    def source_relation(source)
      binding = source[:kind] == :table && !source[:database] && @ctes[source[:name]]
      return super unless binding.is_a?(CTEBinding)
      raise Error.new('0A000', 'data-modifying CTE has no RETURNING clause') if binding.side_effect
      body = respond_to?(:explain_cte_source_query) ? explain_cte_source_query(source, binding) : source[:native_body_query] || binding.query
      if binding.query
        previous_ctes, previous_outer = @ctes, @outer
        begin
          @ctes, @outer = binding.ctes.dup, binding.outer.dup
          relation = select(body)
        ensure
          @ctes, @outer = previous_ctes, previous_outer
        end
      else
        relation = binding
        charge(relation.rows.length)
      end
      aliases = source[:columns] || source[:column_aliases]
      raise Error.new('42P10', 'too many relation column names') if aliases && aliases.length > binding.columns.length
      columns = binding.columns.each_with_index.map do |column, index|
        column.merge(table: source[:alias] || source[:name], name: aliases&.[](index) || column[:name], source_ordinal: index)
      end
      result = Relation.new(columns: columns, rows: relation.rows)
      query_memory_transfer(relation, result) if @query_memory && binding.query
      result
    end

    def insert(statement)
      return super unless @cte_read_tables && !@cte_mutation
      with_cte_mutation(statement) { super }
    end

    def update(statement)
      return super unless @cte_read_tables && !@cte_mutation
      with_cte_mutation(statement) { super }
    end

    def delete(statement)
      return super unless @cte_read_tables && !@cte_mutation
      with_cte_mutation(statement) { super }
    end

    def enforce_references(table, changes, operation, *rest)
      merge_cte_write if @cte_mutation && !@cte_mutation[:merged]
      super
    end

    private

    def validate_cte_structure(node, top_level = false)
      case node
      when Array then node.each { |child| validate_cte_structure(child) }
      when Hash
        (node[:with] || []).each do |cte|
          body = cte[:query]
          if !top_level && %i[insert update delete].include?(body[:kind])
            raise Error.new('0A000', 'data-modifying CTE must be at statement top level')
          end
          if (cte[:search] || cte[:cycle]) && (!node[:recursive] || cte_reference_count(body, cte[:name]).zero?)
            raise Error.new('42601', 'SEARCH/CYCLE requires a recursive CTE')
          end
          validate_recursive_shape(cte) if node[:recursive] && cte_reference_count(body, cte[:name]).positive?
        end
        node.each do |key, child|
          validate_cte_structure(child, key == :query && %i[create_table create_table_as].include?(node[:kind]))
        end
      end
    end

    def validate_recursive_shape(cte)
      body, name = cte[:query], cte[:name]
      unless %i[set_operation setop].include?(body[:kind]) && body[:op] == 'union'
        raise Error.new('42P19', 'recursive CTE requires an anchor UNION recursive term')
      end
      raise Error.new('42P19', 'recursive reference in anchor') if cte_reference_count(body[:left], name).positive?
      recursive = body[:right]
      if %i[set_operation setop].include?(recursive[:kind])
        raise Error.new(cte[:search] || cte[:cycle] ? '42601' : '0A000', 'set operation in recursive term')
      end
      if (cte[:search] || cte[:cycle]) && %i[set_operation setop].include?(body[:left][:kind])
        raise Error.new('0A000', 'SEARCH/CYCLE requires one anchor SELECT')
      end
      sources = (recursive[:from] || []) + (recursive[:joins] || []).map { |join| join[:relation] }
      visit_view_nodes(recursive) do |node|
        if %i[subquery exists array_query in quantified].include?(node[:kind]) && node[:query] && cte_reference_count(node[:query], name).positive?
          raise Error.new('0A000', 'recursive reference in derived table') if sources.any? { |source| source.equal?(node) }
          raise Error.new('42P19', 'recursive reference in expression subquery')
        end
      end
      # A FROM-derived relation shares the :subquery tag with scalar expressions.
      ((recursive[:from] || []) + (recursive[:joins] || []).map { |join| join[:relation] }).each do |source|
        if source[:kind] == :subquery && cte_reference_count(source[:query], name).positive?
          raise Error.new('0A000', 'recursive reference in derived table')
        end
      end
      raise Error.new('42P19', 'recursive term must reference itself once') unless cte_reference_count(recursive, name) == 1
      raise Error.new('42P19', 'aggregate in recursive term') if contains_aggregate?(recursive)
      left_self = (recursive[:from] || []).any? { |source| cte_reference_count(source, name).positive? }
      (recursive[:joins] || []).each do |join|
        right_self = cte_reference_count(join[:relation], name).positive?
        if right_self && %i[left full].include?(join[:kind]) || left_self && %i[right full].include?(join[:kind])
          raise Error.new('42P19', 'recursive reference on nullable outer-join side')
        end
        left_self ||= right_self
      end
      if !(body[:order_by] || []).empty? || body[:limit] || body[:offset]
        raise Error.new('0A000', 'ordering or limiting a recursive query is not implemented')
      end
    end

    def materialize_recursive_cte(cte)
      body = cte[:query]
      relation = select(body[:left])
      metadata = view_query_columns(body[:left])
      if cte[:columns] && cte[:columns].length > relation.columns.length
        raise Error.new('42P10', 'too many CTE column names')
      end
      columns = relation.columns.each_with_index.map { |column, index| column.merge(name: cte[:columns]&.[](index) || column[:name], type_mod: metadata[index]&.dig(:type_mod)) }
      added = cte_search_cycle_columns(cte, columns)
      recursive_scope = columns + added.map { |column| column.merge(star_hidden: true) }
      @ctes[cte[:name]] = Relation.new(columns: recursive_scope, rows: relation.rows)
      recursive_columns = view_query_columns(body[:right], nil, @outer.flat_map(&:first))
      raise Error.new('42601', 'recursive query column count differs') unless columns.length == recursive_columns.length
      columns.zip(recursive_columns).each do |anchor, recursive|
        if common_type(anchor[:type], recursive[:type]) != anchor[:type] || anchor[:type_mod] && anchor[:type_mod] != recursive[:type_mod]
          raise Error.new('42804', 'recursive term widens the anchor column type')
        end
        require_set_equality(anchor[:type]) unless body[:all]
      end
      anchor_expressions, step_expressions, cycle_filter = cte_generated_expressions(cte, columns, added)
      rows = relation.rows.map { |row| row + anchor_expressions.map { |expression| evaluate(expression, row) } }
      rows = rows.uniq unless body[:all]
      if @query_memory
        result_owner = query_memory_owner
        dedup_owner = query_memory_owner unless body[:all]
        rows.each do |row|
          query_memory_add(dedup_owner, MemorySchedule::ENTRY + query_memory_key_bytes(row)) if dedup_owner
          query_memory_add_row(result_owner, row, recursive_scope)
        end
        working_owner = relation.instance_variable_get(:@native_memory_owner)
      end
      working = rows
      seen = rows.to_h { |row| [row, true] }
      recursive_query = body[:right]
      unless added.empty?
        sources = (recursive_query[:from] || []) + (recursive_query[:joins] || []).map { |join| join[:relation] }
        source = sources.find { |item| item[:kind] == :table && item[:name] == cte[:name] }
        extras = added.map { |column| { expr: { kind: :column, table: source[:alias] || source[:name], name: column[:name] } } }
        recursive_query = recursive_query.merge(columns: recursive_query[:columns] + extras)
      end
      until working.empty?
        @ctes[cte[:name]] = Relation.new(columns: recursive_scope, rows: working)
        next_relation = select(recursive_query)
        query_memory_release(working_owner) if @query_memory
        working = next_relation.rows.filter_map do |row|
          own = row.first(columns.length).each_with_index.map { |value, index| cast_value(value, columns[index][:type]) }
          context = own + row.drop(columns.length)
          next if cycle_filter && evaluate(cycle_filter, context) != true
          own + step_expressions.map { |expression| evaluate(expression, context) }
        end
        unless body[:all]
          working = working.reject { |row| seen.key?(row) }.uniq
          working.each { |row| seen[row] = true }
        end
        if @query_memory
          working_owner = next_relation.instance_variable_get(:@native_memory_owner)
          retained = working.sum { |row| query_memory_row_bytes(row, recursive_scope) }
          query_memory_release(working_owner, working_owner.bytes - retained) if working_owner && working_owner.bytes > retained
          working.each do |row|
            query_memory_add(dedup_owner, MemorySchedule::ENTRY + query_memory_key_bytes(row)) if dedup_owner
            query_memory_add_row(result_owner, row, recursive_scope)
          end
        end
        rows.concat(working)
      end
      @ctes[cte[:name]] = Relation.new(columns: columns + added, rows: rows)
      if @query_memory
        query_memory_release(working_owner)
        query_memory_release(dedup_owner)
        query_memory_attach(@ctes[cte[:name]], result_owner)
      end
    end

    def cycle_mark_values(cycle)
      values = [cycle[:value], cycle[:default]].map do |node|
        if node[:kind] == :literal
          type = Types.normalize(node[:type])
          [type == 'decimal' ? cast_value(node[:value], type) : node[:value], type, node[:untyped]]
        elsif node[:kind] == :cast && node.dig(:expr, :kind) == :literal
          type = Types.normalize(node[:type])
          [cast_value(node[:expr][:value], type), type, false]
        else raise Error.new('42601', 'CYCLE marks must be unsigned literal constants')
        end
      end
      left, right = values
      left[1] = right[1] if left[2] && left[1] == 'text' && right[1] != 'unknown'
      right[1] = left[1] if right[2] && right[1] == 'text' && left[1] != 'unknown'
      type = common_type(left[1], right[1])
      type = 'text' if type == 'unknown'
      require_set_equality(type)
      [type, cast_value(left[0], type), cast_value(right[0], type)]
    end

    def cte_generated_expressions(cte, columns, added)
      return [[], [], nil] if added.empty?
      scope = columns + added.map { |column| column.merge(table: '__parent') }
      key_row = lambda { |names| { kind: :row, args: names.map { |name| { kind: :column, name: name } } } }
      parent = lambda { |name| { kind: :column, name: name, table: '__parent' } }
      singleton = lambda { |row| { kind: :array, args: [row] } }
      append = lambda { |name, row| { kind: :binary, op: '||', left: parent.call(name), right: singleton.call(row) } }
      anchor, step = [], []
      if (search = cte[:search])
        row = key_row.call(search[:columns])
        if search[:order] == 'depth'
          anchor << singleton.call(row)
          step << append.call(search[:column], row)
        else
          anchor << row.merge(args: [{ kind: :literal, value: 0, type: 'i64' }] + row[:args])
          depth = { kind: :binary, op: '+', left: { kind: :field, expr: parent.call(search[:column]), name: 'f1' }, right: { kind: :literal, value: 1, type: 'i64' } }
          step << row.merge(args: [depth] + row[:args])
        end
      end
      filter = nil
      if (cycle = cte[:cycle])
        type, yes_value, no_value = cycle_mark_values(cycle)
        yes = { kind: :literal, type: type, value: yes_value }
        no = { kind: :literal, type: type, value: no_value }
        row = key_row.call(cycle[:columns])
        condition = { kind: :quantified, op: '=', quantifier: 'any', left: row, right: parent.call(cycle[:path]) }
        anchor.concat([no, singleton.call(row)])
        step.concat([{ kind: :case, branches: [[condition, yes]], else: no }, append.call(cycle[:path], row)])
        filter = compile({ kind: :binary, op: '<>', left: parent.call(cycle[:mark]), right: yes }, scope)
      end
      [anchor.map { |node| compile(node, columns) }, step.map { |node| compile(node, scope) }, filter]
    end

    def project_mutation(statement, table, operation, changes, scope, expressions, columns)
      if @cte_mutation && !@cte_mutation[:merged]
        @cte_mutation[:changes] = changes
        @cte_mutation[:name] = table[:name]
        @cte_mutation[:before] = table
      end
      super
    end

    def with_cte_mutation(statement)
      previous_tables = @tables
      @tables = @cte_working_tables.dup
      name = statement[:table]
      # Views delegate to their underlying table while this wrapper remains in
      # scope; their target is found when project_mutation captures the changes.
      @tables[name] = @cte_read_tables[name] if @cte_read_tables.key?(name)
      @cte_mutation = { working: @cte_working_tables }
      result = yield
      merge_cte_write unless @cte_mutation[:merged]
      @cte_working_tables = @tables
      result
    ensure
      @cte_mutation = nil
      @tables = previous_tables.equal?(@cte_read_tables) ? @cte_read_tables : @cte_working_tables
    end

    def merge_cte_write
      context = @cte_mutation
      context[:merged] = true
      return unless context[:name]
      before = context[:before]
      name = context[:name]
      working = context[:working][name]
      changes = context[:changes] || []
      if changes.empty?
        @tables[name] = working
        return
      end
      rows, rowids = working[:rows].dup, working[:rowids].dup
      next_id = working[:next_rowid]
      before_ids = before[:rows].each_with_index.to_h { |row, index| [row.object_id, before[:rowids][index]] }
      changes.each do |old_row, new_row|
        if old_row
          id = before_ids.fetch(old_row.object_id)
          index = rowids.index(id)
          if new_row
            if index
              rows[index] = new_row
            else
              rows << new_row
              rowids << id
            end
          elsif index
            rows.delete_at(index)
            rowids.delete_at(index)
          end
        else
          rows << new_row
          rowids << next_id
          next_id += 1
        end
      end
      result = @tables[name].merge(rows: rows, rowids: rowids, next_rowid: next_id)
      validate_rows(result.merge(constraints: result[:constraints].reject { |constraint| constraint[:kind] == :check }), rows)
      @tables[name] = result
      @cte_working_tables = @tables
    end

    def writable_with?(statement)
      return writable_with?(statement[:query]) if statement[:kind] == :create_table && statement[:query]
      items = statement[:with] || []
      !items.empty? && (%i[insert update delete].include?(statement[:kind]) || items.any? { |cte| %i[insert update delete].include?(cte[:query][:kind]) })
    end

    def cte_reference_count(node, name)
      case node
      when Array then node.sum { |child| cte_reference_count(child, name) }
      when Hash
        return node[:name] == name && !node[:database] ? 1 : 0 if node[:kind] == :table
        if node[:with]
          count = 0
          node[:with].each do |cte|
            count += cte_reference_count(cte[:query], name)
            return count if cte[:name] == name
          end
          count + cte_reference_count(node.reject { |key, _| key == :with }, name)
        else
          node.values.sum { |child| cte_reference_count(child, name) }
        end
      else 0
      end
    end
  end
  Engine.prepend(CTEs)
end
