# frozen_string_literal: true

module JedNative
  # Mutations build a private end state. Expressions and RETURNING read the
  # pre-statement table until validation finishes and the replacement is published.
  module Mutations
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
      defaults = t[:columns].map { |c| c[:default] && compile(c[:default], []) }
      source = stmt[:query] && select(stmt[:query])
      inputs = source ? source.rows : stmt[:rows]
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
      rows, rowids = t[:rows].dup, t[:rowids].dup
      next_id = t[:next_rowid]
      changes, affected = [], {}
      inputs.each do |input|
        unless stmt[:default_values] || input.length == indices.length
          raise Error.new('42601', 'INSERT column count mismatch')
        end
        supplied = indices.zip(input).to_h
        candidate = t[:columns].each_with_index.map do |column, ordinal|
          node = supplied[ordinal]
          if stmt[:default_values] || !supplied.key?(ordinal) || column[:identity] && stmt[:overriding] == 'user' || !source && node[:kind] == :default
            expr = defaults[ordinal]
            value = expr && evaluate(expr, [])
          elsif source
            value = node
            expr = Expression.new(kind: :source, type: source.columns[indices.index(ordinal)][:type])
          else
            expr = compile(node, [])
            value = evaluate(expr, [])
          end
          assign(value, column.merge(nullable: true), expr)
        end
        validate_candidate(t, candidate, checks)
        target = conflict && conflict_row(arbiters, candidate, rows)
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
          rows << candidate
          rowids << next_id
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
      charge unless candidates || t[:rows].empty?
      selected = candidates&.to_h { |row| [row.object_id, true] }
      rows = t[:rows].map do |row|
        next row if selected && !selected[row.object_id]
        charge
        next row if predicate && evaluate(predicate, row) != true
        replacement = apply_assignments(t, row, row, assignments)
        validate_candidate(t, replacement, checks)
        changes << [row, replacement]
        replacement
      end
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
      changes, rows, rowids = [], [], []
      candidates = primary_scan(t, predicate)
      charge unless candidates || t[:rows].empty?
      selected = candidates&.to_h { |row| [row.object_id, true] }
      t[:rows].each_with_index do |row, index|
        if selected && !selected[row.object_id]
          rows << row
          rowids << t[:rowids][index]
          next
        end
        charge
        if !predicate || evaluate(predicate, row) == true
          changes << [row, nil]
        else
          rows << row
          rowids << t[:rowids][index]
        end
      end
      result = project_mutation(stmt, t, :delete, changes, return_scope, return_exprs, return_columns)
      unless changes.empty?
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

    def apply_assignments(t, original, context, assignments)
      replacement = original.dup
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
        compile_filter(check[:expr], scope)
      end
    end

    def validate_candidate(t, row, checks)
      t[:columns].each_with_index do |column, index|
        raise Error.new('23502', "NULL in #{column[:name]}") if row[index].nil? && column[:nullable] == false
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
      rows = changes.map do |before, after|
        current = operation == :delete ? before : after
        context = current.dup
        scope.drop(t[:columns].length).each_slice(t[:columns].length) do |group|
          context.concat((group.first[:row_version] == :old ? before : after) || null_row)
        end
        values = expressions.map { |expression| evaluate(expression, context) }
        charge
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
        names = conflict[:elements].map do |element|
          node = element[:kind] == :collate ? element[:expr] : element
          raise Error.new('42P10', 'conflict target must name columns') unless node[:kind] == :column
          mutation_target_indices(t, [node[:name]])
          raise Error.new('42704', 'collation is not loaded') if element[:kind] == :collate && element[:name] != 'C'
          raise Error.new('0A000', 'conflict operator classes are not implemented') if element[:opclass]
          node[:name]
        end.uniq.sort
        compile(conflict[:predicate], scope) if conflict[:predicate]
        arbiters.select! do |arbiter|
          arbiter[:kind] != :exclude && arbiter[:columns]&.compact&.sort == names &&
            (!arbiter[:where] || predicate_implies?(conflict[:predicate], arbiter[:where]))
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

    def predicate_implies?(provided, required)
      return false unless provided
      return true if provided == required
      if provided[:kind] == :binary && provided[:op] == 'and'
        return predicate_implies?(provided[:left], required) || predicate_implies?(provided[:right], required)
      end
      false
    end

    def conflict_row(arbiters, candidate, rows)
      arbiters.each do |arbiter|
        next if arbiter[:predicate] && evaluate(arbiter[:predicate], candidate) != true
        key = arbiter[:expressions].map { |expr| evaluate(expr, candidate) }
        next if key.any?(&:nil?)
        rows.each_with_index do |row, index|
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
      nil
    end
  end
  Engine.prepend(Mutations)
end
