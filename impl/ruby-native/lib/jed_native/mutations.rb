# frozen_string_literal: true

module JedNative
  # Mutations build a private end state. Expressions and RETURNING read the
  # pre-statement table until validation finishes and the replacement is published.
  module Mutations
    INSERT_DEFAULT = Object.new.freeze
    def insert(stmt)
      t = table(stmt[:table])
      if stmt[:default_values] && (stmt[:columns] || stmt[:overriding])
        raise Error.new('42601', 'DEFAULT VALUES does not accept a column list or OVERRIDING')
      end
      names = stmt[:columns] || t[:columns].map { |c| c[:name] }
      indices = mutation_target_indices(t, names)
      indices.each_with_index do |ordinal, position|
        next unless t[:columns][ordinal][:identity] == :always
        next if %w[system user].include?(stmt[:overriding])
        if stmt[:query] || (stmt[:rows] || []).any? { |row| row[position] && row[position][:kind] != :default }
          raise Error.new('428C9', 'GENERATED ALWAYS requires DEFAULT or OVERRIDING')
        end
      end
      defaults = t[:columns].each_with_index.map do |column, ordinal|
        position = indices.index(ordinal)
        needed = stmt[:default_values] || position.nil? || column[:identity] && stmt[:overriding] == 'user' ||
                 (stmt[:rows] || []).any? { |row| row[position]&.dig(:kind) == :default }
        column[:default] && needed ? compile(column[:default], []) : nil
      end
      source = stmt[:query] && select(stmt[:query])
      inputs = source ? source.rows : stmt[:rows]
      # Resolve every VALUES slot first so its uncorrelated subqueries fold in
      # row-major order. Materialize all rows before RETURNING and conflict
      # subqueries are allowed to run (cost.md §3).
      prepared = if source
        inputs.each do |input|
          raise Error.new('42601', 'INSERT column count mismatch') unless input.length == indices.length
        end
        # Source types are identical for every row. Keep one descriptor per
        # column instead of allocating rows × columns identical expressions.
        source.columns.map { |column| Expression.new(kind: :source, type: column[:type]) }
      else
        inputs.map do |input|
          unless stmt[:default_values] || input.length == indices.length
            raise Error.new('42601', 'INSERT column count mismatch')
          end
          input.each_with_index.map do |node, position|
            column = t[:columns][indices[position]]
            if stmt[:default_values] || node[:kind] == :default || column[:identity] && stmt[:overriding] == 'user'
              nil
            else
              compile(node, [])
            end
          end
        end
      end
      assignment_columns = t[:columns].map { |column| column.merge(nullable: true) }
      unassigned = (0...t[:columns].length).to_a - (stmt[:default_values] ? [] : indices)
      candidates = inputs.each_with_index.map do |input, row_index|
        values_start = @cost if @explain_profile_frames
        candidate = Array.new(t[:columns].length)
        indices.each_with_index do |ordinal, position|
          next if stmt[:default_values]
          column = t[:columns][ordinal]
          expr = source ? prepared[position] : prepared[row_index][position]
          if column[:identity] && stmt[:overriding] == 'user' || !source && input[position][:kind] == :default
            candidate[ordinal] = INSERT_DEFAULT
          else
            value = source ? input[position] : assignment_value(expr)
            candidate[ordinal] = assign(value, assignment_columns[ordinal], expr)
          end
        end
        unassigned.each { |ordinal| candidate[ordinal] = INSERT_DEFAULT }
        profile_accumulate(:values, @cost - values_start) if @explain_profile_frames
        candidate
      end
      conflict = stmt[:on_conflict]
      scope = table_columns(t, stmt[:alias] || t[:name])
      return_scope, return_exprs, return_columns = mutation_returning(stmt, t, :insert)
      if conflict
        arbiters = conflict_arbiters(t, conflict)
        update_scope = scope + table_columns(t, 'excluded').map { |c| c.merge(qualified_only: true) }
        assignments = resolve_assignments(t, conflict[:assignments] || [], update_scope)
        predicate = compile_filter(conflict[:where], update_scope)
      end
      checks = mutation_checks(t)
      paged = t[:rows].is_a?(PagedRows)
      rows = paged ? PendingRows.new(t) : t[:rows].dup
      rowids = paged ? rows.rowids : t[:rowids].dup
      next_id = t[:next_rowid]
      changes, affected = [], {}
      candidates.each do |candidate|
        # Defaults retain their per-candidate schedule: sequence defaults in an
        # ON CONFLICT update interleave with the next candidate's defaults.
        candidate.each_index do |ordinal|
          next unless candidate[ordinal].equal?(INSERT_DEFAULT)
          expr = defaults[ordinal]
          value = expr && assignment_value(expr)
          candidate[ordinal] = assign(value, assignment_columns[ordinal], expr)
        end
        validate_candidate(t, candidate, checks)
        target = conflict && conflict_row(arbiters, candidate, rows, affected)
        if target
          next if conflict[:action] == :nothing
          raise Error.new('21000', 'ON CONFLICT cannot affect a row twice') if affected[target]
          existing = rows[target]
          context = existing + candidate
          next if predicate && evaluate(predicate, context) != true
          replacement = apply_assignments(t, existing, context, assignments)
          validate_candidate(t, replacement, checks)
          validate_view_candidate(t, replacement) if respond_to?(:validate_view_candidate, true)
          rows[target] = replacement
          affected[target] = true
          changes << [existing, replacement]
        else
          if !conflict && respond_to?(:view_checks_pending?, true) && view_checks_pending?
            validate_rows(t.merge(constraints: t[:constraints].select { |c| %i[primary_key unique].include?(c[:kind]) }), rows + [candidate])
          end
          validate_view_candidate(t, candidate) if respond_to?(:validate_view_candidate, true)
          affected[rows.length] = true
          if paged
            rows.append(candidate, next_id)
          else
            rows << candidate
            rowids << next_id
          end
          next_id += 1
          changes << [nil, candidate]
        end
      end
      validate_rows(t.merge(constraints: t[:constraints].reject { |c| c[:kind] == :check }), rows)
      result = project_mutation(stmt, t, :insert, changes, return_scope, return_exprs, return_columns)
      unless changes.empty?
        @tables[t[:name]] = t.merge(rows: rows, rowids: rowids, next_rowid: next_id)
        enforce_references(t, changes.select(&:first), :update)
        @dirty = true
      end
      result
    end

    def update(stmt)
      raise Error.new('0A000', 'UPDATE FROM is not implemented') if stmt[:from] && !stmt[:from].empty?
      t = table(stmt[:table])
      scope = table_columns(t, stmt[:alias] || t[:name])
      assignments = resolve_assignments(t, stmt[:assignments], scope)
      predicate = compile_filter(stmt[:where], scope)
      return_scope, return_exprs, return_columns = mutation_returning(stmt, t, :update)
      checks = mutation_checks(t)
      changes = []
      candidates = primary_scan(t, predicate)
      unless candidates
        charge(full_scan_pages(t))
        charge_scan_payload(t)
      end
      paged = t[:rows].is_a?(PagedRows)
      positions = !paged && candidates && t[:rows].each_with_index.to_h { |row, index| [row.object_id, index] }
      profile_checkpoint(:scan) if @explain_profile_frames
      rows = paged ? PendingRows.new(t) : t[:rows].dup
      (candidates || t[:rows]).each_with_index do |row, index|
        charge
        profile_accumulate(:scan, 1) if @explain_profile_frames
        filter_start = @cost if @explain_profile_frames
        accepted = !predicate || evaluate(predicate, row) == true
        profile_accumulate(:filter, @cost - filter_start) if @explain_profile_frames
        next unless accepted
        replacement = apply_assignments(t, row, row, assignments, retain: true)
        validate_candidate(t, replacement, checks)
        changes << [row, replacement]
        if paged
          rows.replace(row, replacement)
        else
          rows[positions ? positions.fetch(row.object_id) : index] = replacement
        end
      end
      profile_inherit(:filter, :scan) if @explain_profile_frames
      validate_rows(t.merge(constraints: t[:constraints].reject { |c| c[:kind] == :check }), rows)
      changes.each { |_, replacement| validate_view_candidate(t, replacement) } if respond_to?(:validate_view_candidate, true)
      result = project_mutation(stmt, t, :update, changes, return_scope, return_exprs, return_columns)
      unless changes.empty?
        @tables[t[:name]] = t.merge(rows: rows)
        enforce_references(t, changes, :update)
        @dirty = true
      end
      result
    end

    def delete(stmt)
      raise Error.new('0A000', 'DELETE USING is not implemented') if stmt[:using] && !stmt[:using].empty?
      t = table(stmt[:table])
      scope = table_columns(t, stmt[:alias] || t[:name])
      predicate = compile_filter(stmt[:where], scope)
      return_scope, return_exprs, return_columns = mutation_returning(stmt, t, :delete)
      changes, removed = [], {}
      candidates = primary_scan(t, predicate)
      unless candidates
        charge(full_scan_pages(t))
        charge_scan_payload(t)
      end
      paged = t[:rows].is_a?(PagedRows)
      positions = !paged && candidates && t[:rows].each_with_index.to_h { |row, index| [row.object_id, index] }
      profile_checkpoint(:scan) if @explain_profile_frames
      (candidates || t[:rows]).each_with_index do |row, index|
        charge
        profile_accumulate(:scan, 1) if @explain_profile_frames
        filter_start = @cost if @explain_profile_frames
        accepted = !predicate || evaluate(predicate, row) == true
        profile_accumulate(:filter, @cost - filter_start) if @explain_profile_frames
        if accepted
          changes << [row, nil]
          removed[positions ? positions.fetch(row.object_id) : index] = true
        end
      end
      profile_inherit(:filter, :scan) if @explain_profile_frames
      result = project_mutation(stmt, t, :delete, changes, return_scope, return_exprs, return_columns)
      unless changes.empty?
        if paged
          rows = PendingRows.new(t)
          changes.each { |row, _| rows.replace(row, nil) }
          rowids = rows.rowids
        else
          rows, rowids = [], []
          t[:rows].each_with_index do |row, index|
            next if removed[index]
            rows << row
            rowids << t[:rowids][index]
          end
        end
        @tables[t[:name]] = t.merge(rows: rows, rowids: rowids)
        enforce_references(t, changes, :delete)
        @dirty = true
      end
      result
    end

    private

    def mutation_target_indices(t, names)
      raise Error.new('42701', 'duplicate target column') unless names.uniq.length == names.length
      names.map { |name| t[:columns].index { |c| c[:name] == name } || raise(Error.new('42703', "unknown column #{name}")) }
    end

    def resolve_assignments(t, assignments, scope)
      names = assignments.flat_map { |assignment| assignment[:columns] || [assignment[:column]] }
      mutation_target_indices(t, names)
      assignments.map do |assignment|
        targets = mutation_target_indices(t, assignment[:columns] || [assignment[:column]])
        raise Error.new('0A000', 'subscript assignment is not implemented') if assignment[:subscripts]
        node = assignment[:expr]
        if node[:kind] != :default && targets.any? { |i| t[:columns][i][:identity] == :always }
          raise Error.new('428C9', 'GENERATED ALWAYS requires DEFAULT')
        end
        if node[:kind] == :default
          raise Error.new('42601', 'DEFAULT requires one target column') unless targets.length == 1
          node = t[:columns][targets[0]][:default] || { kind: :literal, type: 'unknown', value: nil }
          expr = compile(node, [])
        else
          expr = compile(node, scope)
        end
        [targets, expr]
      end
    end

    def mutation_copy_row(table, original)
      return original.dup unless original.respond_to?(:storage_reference)
      names = table[:constraints].find { |constraint| constraint[:kind] == :primary_key }&.dig(:columns) || []
      excluded = table[:columns].each_index.select { |index| names.include?(table[:columns][index][:name]) }
      CarriedRow.new(original, excluded)
    end

    def apply_assignments(t, original, context, assignments, retain: false)
      replacement = retain ? mutation_copy_row(t, original) : original.dup
      assignments.each do |targets, expression|
        value = evaluate(expression, context)
        values = targets.length == 1 ? [value] : value
        raise Error.new('42601', 'row assignment has wrong width') unless values.is_a?(Array) && values.length == targets.length
        targets.each_with_index do |ordinal, index|
          source = targets.length == 1 ? expression : expression.args&.at(index)
          replacement[ordinal] = assign(values[index], t[:columns][ordinal].merge(nullable: true), source)
        end
      end
      replacement
    end

    def mutation_checks(t)
      scope = table_columns(t)
      t[:constraints].select { |c| c[:kind] == :check }.sort_by { |c| c[:name].to_s.b }.map do |check|
        compile_boolean_value(check[:expr], scope)
      end
    end

    def validate_candidate(t, row, checks)
      t[:columns].each_with_index do |column, index|
        next unless column[:nullable] == false
        absent = row.respond_to?(:null_at?) ? row.null_at?(index) : row[index].nil?
        raise Error.new('23502', "NULL in #{column[:name]}") if absent
      end
      checks.each { |expression| raise Error.new('23514', 'check constraint violation') if evaluate(expression, row) == false }
    end

    def mutation_returning(stmt, t, operation)
      return [nil, nil, nil] unless stmt[:returning]
      name = stmt[:alias] || t[:name]
      explicit = stmt[:returning_aliases] || {}
      aliases = { old: 'old', new: 'new' }.reject { |version, label| !explicit.key?(version) && explicit.value?(label) }.merge(explicit)
      if explicit.values.uniq.length != explicit.length || explicit.values.include?(name)
        raise Error.new('42712', 'duplicate RETURNING alias')
      end
      scope = table_columns(t, name)
      %i[old new].each do |version|
        scope.concat(table_columns(t, aliases[version]).map { |c| c.merge(qualified_only: true, row_version: version) }) if aliases[version] && aliases[version] != name
      end
      projections = expand_stars(stmt[:returning], scope)
      expressions = projections.map { |projection| compile(projection[:expr], scope) }
      raise Error.new('42803', 'aggregate in RETURNING') if expressions.any? { |expr| aggregate_expr?(expr) }
      columns = projections.zip(expressions).map { |p, e| { name: p[:alias] || expression_name(p[:expr]), type: e.type } }
      [scope, expressions, columns]
    end

    def project_mutation(stmt, t, operation, changes, scope, expressions, columns)
      return Result.new(rows_affected: changes.length) unless expressions
      null_row = Array.new(t[:columns].length)
      memory_owner = query_memory_owner if @query_memory
      rows = changes.map do |before, after|
        current = operation == :delete ? before : after
        segments = [current]
        scope.drop(t[:columns].length).each_slice(t[:columns].length) do |group|
          segments << ((group.first[:row_version] == :old ? before : after) || null_row)
        end
        context = MutationFrame.new(segments)
        values = expressions.map { |expression| evaluate(expression, context) }
        charge
        query_memory_add_row(memory_owner, values, columns) if @query_memory
        values
      end
      Result.new(columns: columns.map { |c| c[:name] }, column_types: columns.map { |c| c[:type] }, rows: rows, rows_affected: changes.length)
    end

    def conflict_arbiters(t, conflict)
      scope = table_columns(t)
      arbiters = t[:constraints].filter_map do |constraint|
        kind = constraint[:kind]
        next unless %i[primary_key unique exclude].include?(kind)
        name = constraint[:name] || (kind == :primary_key ? "#{t[:name]}_pkey" : "#{t[:name]}_#{constraint[:columns]&.join('_')}_key")
        elements = kind == :exclude ? constraint[:elements] : constraint[:columns].map { |n| { expr: { kind: :column, name: n }, op: '=' } }
        { name: name, kind: kind, columns: constraint[:columns], elements: elements, where: constraint[:where] }
      end
      t[:indexes].select { |index| index[:unique] }.each do |index|
        elements = index[:elements] || index[:columns].map { |n| { expr: { kind: :column, name: n } } }
        names = elements.map { |element| element[:expr][:name] if element[:expr][:kind] == :column }
        arbiters << { name: index[:name], kind: :unique, columns: names, elements: elements, where: index[:where] }
      end
      if conflict[:constraint]
        arbiters.select! { |arbiter| arbiter[:name] == conflict[:constraint] }
        raise Error.new('42704', 'unknown conflict constraint') if arbiters.empty?
        raise Error.new('42809', 'exclusion arbiter cannot update') if conflict[:action] == :update && arbiters.any? { |a| a[:kind] == :exclude }
      elsif conflict[:elements]
        decorations_match = true
        names = conflict[:elements].map do |element|
          name, matches = resolve_conflict_element(element, t[:columns])
          decorations_match &&= matches
          name
        end.uniq.sort
        if conflict[:predicate]
          schema_expression_structure(conflict[:predicate], :conflict)
          premise = compile(conflict[:predicate], scope)
        end
        arbiters.select! do |arbiter|
          decorations_match && arbiter[:kind] != :exclude && arbiter[:columns]&.compact&.sort == names &&
            (!arbiter[:where] || partial_predicate_implied?(compile(arbiter[:where], scope), premise ? conjunctions(premise) : []))
        end
        raise Error.new('42P10', 'no matching unique conflict constraint') if arbiters.empty?
        arbiters.sort_by! { |arbiter| [arbiter[:kind] == :primary_key ? 0 : arbiter[:where] ? 2 : 1, arbiter[:name].b] }
        arbiters = arbiters.take(1)
      elsif conflict[:action] == :update
        raise Error.new('42601', 'DO UPDATE requires a conflict target')
      end
      arbiters.map do |arbiter|
        arbiter.merge(expressions: arbiter[:elements].map { |element| compile(element[:expr], scope) },
                      predicate: compile_filter(arbiter[:where], scope))
      end
    end

    def resolve_conflict_element(element, columns)
      raise Error.new('42P10', 'ordering is not allowed in ON CONFLICT') if element[:direction] || element[:nulls]
      node = element[:kind] == :collate ? element[:expr] : element
      raise Error.new('42P10', 'conflict target must name columns') unless node[:kind] == :column
      column = columns.find { |candidate| candidate[:name] == node[:name] }
      raise Error.new('42703', "unknown column #{node[:name]}") unless column
      matches = true
      if element[:kind] == :collate
        resolve_collation(element[:name])
        matches &&= collatable_type?(column[:type]) && (column[:collation] || 'C') == element[:name]
      end
      if element[:opclass]
        opclass = element[:opclass] == 'varchar_ops' ? 'text_ops' : element[:opclass]
        unless FunctionCatalog::BTREE_OPCLASSES.value?(opclass) || %w[array_ops range_ops record_ops].include?(opclass)
          raise Error.new('42704', "operator class #{opclass} does not exist for btree")
        end
        type = column[:type]
        actual = type.end_with?('[]') ? 'array_ops' : Types::RANGES.key?(type) ? 'range_ops' : @types.key?(type) ? 'record_ops' : FunctionCatalog::BTREE_OPCLASSES[type]
        matches &&= opclass == actual
      end
      [node[:name], matches]
    end

    def predicate_implies?(provided, required)
      return false unless provided
      return true if provided == required
      if provided[:kind] == :binary && provided[:op] == 'and'
        return predicate_implies?(provided[:left], required) || predicate_implies?(provided[:right], required)
      end
      false
    end

    def conflict_row(arbiters, candidate, rows, affected = {})
      arbiters.each do |arbiter|
        next if arbiter[:predicate] && evaluate(arbiter[:predicate], candidate) != true
        key = arbiter[:expressions].map { |expr| evaluate(expr, candidate) }
        next if key.any?(&:nil?)
        # Probe unaffected committed rows before the overlay: a new key may
        # temporarily coincide with a committed key that this statement moves.
        [false, true].each do |already_affected|
        rows.each_with_index do |row, index|
          next unless !!affected[index] == already_affected
          next if arbiter[:predicate] && evaluate(arbiter[:predicate], row) != true
          other = arbiter[:expressions].map { |expr| evaluate(expr, row) }
          next if other.any?(&:nil?)
          matches = key.each_index.all? do |position|
            op = arbiter[:elements][position][:op] || '='
            Types.binary(op, key[position], other[position], type: 'boolean') == true
          end
          return index if matches
        end
        end
      end
      nil
    end
  end
  Engine.prepend(Mutations)
end
