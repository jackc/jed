# frozen_string_literal: true

module JedNative
  # Views retain a bound, star-expanded query and its exact catalog dependencies.
  # Planning never executes that query; in particular CREATE VIEW cannot advance
  # a sequence or visit rows which might fail when the view is later read.
  module Views
    def dispatch(statement)
      return alter_view(statement) if statement[:kind] == :alter_view
      if statement[:kind] == :drop_type
        drop_catalog_view_dependencies(:type_dependencies, statement[:names] || [statement[:name]], statement[:cascade])
      end
      super
    end

    def select(statement)
      return statement[:relation] if statement[:kind] == :view_source
      super
    end

    def create_view(statement)
      name = statement[:name]
      raise Error.new('42939', 'reserved relation name') if name.start_with?('jed_')
      return Result.new if statement[:if_not_exists] && relation_name_taken?(name)
      query = statement[:query]
      if statement[:recursive]
        seed = query[:kind] == :set_operation ? query[:left] : query
        raise Error.new('42P10', 'too many recursive view column names') if statement[:columns].length > view_query_columns(seed).length
        query = { kind: :select, columns: statement[:columns].map { |column| { expr: { kind: :column, name: column, quoted: true } } }, from: [{ kind: :table, name: name, quoted: true }],
                  with: [{ name: name, quoted: true, columns: statement[:columns], columns_quoted: Array.new(statement[:columns].length, true), query: query }], recursive: true }
      end
      visit_view_nodes(query) do |node|
        raise Error.new('42P02', 'views cannot contain parameters') if node[:kind] == :param
        raise Error.new('0A000', 'views cannot contain data-modifying CTEs') if %i[insert update delete].include?(node[:kind])
      end
      dependencies = {}
      frozen_query = freeze_view_query(query, {}, [], dependencies)
      schema = view_query_columns(frozen_query)
      options = statement[:options] || {}
      validate_view_options(options)
      raise Error.new('22023', 'CHECK OPTION was specified twice') if options.key?('check_option') && statement[:check_option]
      check_option = statement[:check_option] || options['check_option']&.to_s&.downcase&.to_sym
      candidate = statement.merge(query: frozen_query)
      if check_option && (!updatable_view_shape?(candidate) || frozen_query[:columns].none? { |item| item[:expr][:kind] == :column })
        raise Error.new('0A000', 'CHECK OPTION requires an automatically updatable view')
      end
      names = statement[:columns] || []
      raise Error.new('42601', 'too many view column names') if names.length > schema.length
      schema = schema.each_with_index.map { |column, index| column.merge(name: names[index] || column[:name], type: Types.normalize(column[:type])) }
      raise Error.new('0A000', 'view must have at least one column') if schema.empty?
      raise Error.new('42701', 'duplicate view column') unless schema.map { |column| column[:name] }.uniq.length == schema.length
      raise Error.new('42P16', 'view column has pseudo-type record') if schema.any? { |column| %w[record record[]].include?(column[:type]) }
      schema.each { |column| column[:type] = 'text' if column[:type] == 'unknown' }
      view = statement.merge(query: frozen_query, columns: schema.map { |column| column[:name] }, schema: schema,
                             dependencies: dependencies, defaults: {}, check_option: check_option)
      view[:sequence_dependencies], view[:type_dependencies] = view_catalog_dependencies(frozen_query, schema)
      if (old = @views[name]) && statement[:replace]
        previous = view_schema(old)
        if schema.length < previous.length || previous.each_with_index.any? { |column, index| column[:name] != schema[index][:name] || column[:type] != schema[index][:type] || column[:type_mod] != schema[index][:type_mod] }
          raise Error.new('42P16', 'cannot change existing view columns')
        end
        if view_reaches?(dependencies.keys, name)
          raise Error.new('42P17', 'infinite recursion in view definition')
        end
        view[:defaults] = old[:defaults] || {}
      elsif relation_name_taken?(name)
        raise Error.new(statement[:replace] ? '42809' : '42P07', 'relation already exists')
      end
      @views[name] = view
      @dirty = true
      Result.new
    end

    def source_relation(source)
      view = source[:kind] == :table && @views[source[:name]]
      return super unless view && !@ctes.key?(source[:name])
      names = view_schema(view).map { |column| column[:name] }
      (source[:columns] || []).each_with_index { |name, index| names[index] = name }
      raise Error.new('42P10', 'too many view column aliases') if names.length > view_schema(view).length
      previous_ctes, previous_outer = @ctes, @outer
      @ctes, @outer = {}, []
      begin
        super(source.merge(columns: names))
      ensure
        @ctes, @outer = previous_ctes, previous_outer
      end
    end

    def insert(statement)
      return super unless @views.key?(statement[:table])
      rewrite_view_mutation(statement, :insert) { |rewritten| super(rewritten) }
    end

    def update(statement)
      return super unless @views.key?(statement[:table])
      rewrite_view_mutation(statement, :update) { |rewritten| super(rewritten) }
    end

    def delete(statement)
      return super unless @views.key?(statement[:table])
      rewrite_view_mutation(statement, :delete) { |rewritten| super(rewritten) }
    end

    def compile(node, columns)
      if node[:kind] == :view_filter
        return Expression.new(kind: :view_filter, type: 'boolean', args: [compile_filter(node[:left], columns), compile_filter(node[:right], columns)])
      end
      super
    end

    def evaluate(expression, row, group = nil)
      if expression.kind == :view_filter
        return false unless evaluate(expression.args.first, row, group) == true
        return evaluate(expression.args.last, row, group)
      end
      super
    end

    def table_columns(table, name = table[:name])
      super.each_with_index.map { |column, index| column.merge(type_mod: table[:columns][index][:type_mod]) }
    end

    def drop_view(statement)
      names = (statement[:names] || [statement[:name]]).uniq
      names.each do |name|
        raise Error.new('42809', 'relation is not a view') if !@views.key?(name) && relation_name_taken?(name)
        raise Error.new('42P01', 'view does not exist') unless @views.key?(name) || statement[:if_exists]
      end
      drop_dependent_views(names, statement[:cascade], except: names)
      names.each { |name| @views.delete(name) }
      @dirty = true
      Result.new
    end

    def drop_table(statement)
      names = statement[:names] || [statement[:name]]
      raise Error.new('42809', 'relation is not a table') if names.any? { |name| @views.key?(name) }
      owned = @sequences.select { |_, sequence| sequence[:owned_by] && names.include?(sequence[:owned_by].first) }.keys
      drop_catalog_view_dependencies(:sequence_dependencies, owned, statement[:cascade])
      drop_dependent_views(names, statement[:cascade])
      super
    end

    def drop_sequence(statement)
      drop_catalog_view_dependencies(:sequence_dependencies, statement[:names] || [statement[:name]], statement[:cascade])
      super
    end

    def alter_sequence(statement)
      result = super
      if statement[:action] == :rename
        @views.transform_values! do |view|
          next view unless (view[:sequence_dependencies] || view_catalog_dependencies(view[:query], view_schema(view)).first).include?(statement[:name])
          query = Marshal.load(Marshal.dump(view[:query]))
          visit_view_nodes(query) do |node|
            if node[:kind] == :call && %w[nextval currval setval].include?(node[:name]) && node[:args]&.first&.dig(:kind) == :literal && node[:args].first[:value] == statement[:name]
              node[:args].first[:value] = statement[:new_name]
            end
          end
          view.merge(query: query, sequence_dependencies: (view[:sequence_dependencies] || []).map { |name| name == statement[:name] ? statement[:new_name] : name })
        end
      end
      result
    end

    def alter_table(statement)
      return alter_view(statement) if @views.key?(statement[:name])
      bind_imported_view_dependents(statement[:name])
      original_columns = @tables[statement[:name]]&.dig(:columns)&.map { |column| column[:name] } || []
      (statement[:actions] || []).each do |action|
        if %i[drop_column drop_identity].include?(action[:kind])
          ordinal = @tables[statement[:name]]&.dig(:columns)&.index { |column| column[:name] == action[:column] }
          owned = @sequences.select { |_, sequence| sequence[:owned_by] == [statement[:name], ordinal] }.keys
          drop_catalog_view_dependencies(:sequence_dependencies, owned, action[:cascade])
        end
        next unless action[:kind] == :drop_column
        dependents = @views.select { |_, view| (view_dependencies(view)[statement[:name]] || []).include?(action[:column]) }.keys
        next if dependents.empty?
        raise Error.new('2BP01', 'view depends on column') unless action[:cascade]
        drop_dependent_views(dependents, true, except: dependents)
        dependents.each { |name| @views.delete(name) }
      end
      (statement[:actions] || []).each do |action|
        if action[:kind] == :set_type && @views.any? { |_, view| (view_dependencies(view)[statement[:name]] || []).include?(action[:column]) }
          raise Error.new('0A000', 'view depends on column type')
        end
      end
      result = super
      (statement[:actions] || []).each do |action|
        next unless action[:kind] == :drop_column
        ordinal = original_columns.index(action[:column])
        next unless ordinal
        @views.transform_values! do |view|
          next view unless view_dependencies(view).key?(statement[:name])
          query = Marshal.load(Marshal.dump(view[:query]))
          visit_view_nodes(query) do |node|
            if node[:kind] == :table && node[:name] == statement[:name] && node[:view_bound_relation] && node[:columns]
              node[:columns].delete_at(ordinal)
              node[:columns_quoted]&.delete_at(ordinal)
            end
          end
          view.merge(query: query)
        end
        original_columns.delete_at(ordinal)
      end
      if statement[:action] == :rename
        rewrite_view_relation(statement[:name], statement[:new_name])
      elsif statement[:action] == :rename_column
        rewrite_view_column(statement[:name], statement[:column], statement[:new_name])
      end
      result
    end

    def alter_view(statement)
      name = statement[:name]
      unless (view = @views[name])
        raise Error.new('42809', 'relation is not a view') if relation_name_taken?(name)
        return Result.new if statement[:if_exists]
        raise Error.new('42P01', 'view does not exist')
      end
      bind_imported_view_dependents(name)
      view = view.merge(schema: view_schema(view).map(&:dup), columns: view_schema(view).map { |column| column[:name] }, defaults: (view[:defaults] || {}).dup)
      case statement[:action]
      when :rename
        raise Error.new('42P07', 'relation already exists') if relation_name_taken?(statement[:new_name])
        @views.delete(name)
        view[:name] = statement[:new_name]
        @views[view[:name]] = view
        rewrite_view_relation(name, view[:name])
      when :rename_column
        index = view[:columns].index(statement[:column])
        raise Error.new('42703', 'view column does not exist') unless index
        raise Error.new('42701', 'view column already exists') if view[:columns].include?(statement[:new_name])
        view[:columns][index] = statement[:new_name]
        view[:schema][index][:name] = statement[:new_name]
        if view[:defaults].key?(statement[:column])
          view[:defaults][statement[:new_name]] = view[:defaults].delete(statement[:column])
        end
        @views[name] = view
        rewrite_view_column(name, statement[:column], statement[:new_name])
      else
        (statement[:actions] || []).each do |action|
          raise Error.new('42809', 'operation is not supported on a view') unless %i[set_default drop_default set_options reset_options].include?(action[:kind])
          column = view[:schema].find { |item| item[:name] == action[:column] }
          if %i[set_default drop_default].include?(action[:kind])
            raise Error.new('42703', 'view column does not exist') unless column
            if action[:kind] == :set_default
              visit_view_nodes(action[:expr]) do |node|
                raise Error.new('0A000', 'view defaults cannot reference columns or subqueries') if %i[column subquery exists array_query].include?(node[:kind])
              end
              raise Error.new('42803', 'aggregate in view default') if contains_aggregate?(action[:expr])
              expr = compile(action[:expr], [])
              literal = expr.kind == :literal && expr.extra&.dig(:untyped)
              target = Types.base(column[:type])
              source = Types.base(expr.type)
              compatible = target == source || source == 'unknown' || Types::INTEGER_BITS.key?(target) && Types::INTEGER_BITS.key?(source) || target == 'decimal' && Types::INTEGER_BITS.key?(source)
              raise Error.new('42804', 'view default has incompatible type') unless compatible || literal && source == 'text'
              if expr.kind == :literal && expr.value.nil?
                view[:defaults].delete(column[:name])
              else
                view[:defaults][column[:name]] = action[:expr]
              end
            else
              view[:defaults].delete(column[:name])
            end
          elsif action[:kind] == :set_options
            validate_view_options(action[:options])
            view[:options] = (view[:options] || {}).merge(action[:options])
            if action[:options].key?('check_option')
              raise Error.new('0A000', 'CHECK OPTION requires an updatable view') unless updatable_view_shape?(view)
              view[:check_option] = action[:options]['check_option'].to_s.downcase.to_sym
            end
          else
            view[:options] = (view[:options] || {}).reject { |key, _| action[:options].include?(key) }
            view[:check_option] = nil if action[:options].include?('check_option')
          end
        end
        @views[name] = view
      end
      @dirty = true
      Result.new
    end

    private

    def rewrite_view_mutation(statement, operation)
      view = @views[statement[:table]]
      plan = view_write_plan(view, operation)
      label = statement[:alias] || statement[:table]
      aliases = { old: 'old', new: 'new' }.merge(statement[:returning_aliases] || {})
      qualifiers = [label, *aliases.values, 'excluded']
      rewritten = statement.merge(table: plan[:table], alias: label)
      if operation == :insert
        insert_defaults = plan[:defaults].merge(view[:defaults] || {})
        names = statement[:columns] || view_schema(view).map { |column| column[:name] }
        raise Error.new('42701', 'duplicate target column') unless names.uniq.length == names.length
        rewritten[:columns] = names.map { |name| writable_view_column(plan, name) }
        if statement[:default_values]
          rewritten[:columns] = nil
          if !insert_defaults.empty?
            names = insert_defaults.keys
            rewritten[:columns] = names.map { |name| writable_view_column(plan, name) }
            rewritten[:rows] = [names.map { |name| insert_defaults[name] }]
            rewritten[:default_values] = false
          end
        elsif statement[:rows]
          extra = insert_defaults.keys - names
          rewritten[:columns] += extra.map { |name| writable_view_column(plan, name) }
          rewritten[:rows] = statement[:rows].map do |row|
            raise Error.new('42601', 'INSERT column count mismatch') unless row.length == names.length
            row.each_with_index.map { |node, index| node[:kind] == :default && insert_defaults.key?(names[index]) ? insert_defaults[names[index]] : node } + extra.map { |name| insert_defaults[name] }
          end
        elsif statement[:query]
          extra = insert_defaults.keys - names
          unless extra.empty?
            source = select(statement[:query])
            defaults = extra.map { |name| compile(insert_defaults[name], []) }
            columns = source.columns + extra.each_with_index.map { |name, index| { name: name, type: defaults[index].type } }
            rows = source.rows.map { |row| row + defaults.map { |expression| evaluate(expression, []) } }
            rewritten[:columns] += extra.map { |name| writable_view_column(plan, name) }
            rewritten[:query] = { kind: :view_source, relation: Relation.new(columns: columns, rows: rows) }
          end
        end
        if statement[:on_conflict]
          conflict = statement[:on_conflict].dup
          raise Error.new('42704', 'views have no named constraints') if conflict[:constraint]
          if conflict[:elements]
            conflict[:elements] = conflict[:elements].map { |node| rewrite_view_value(node, plan[:columns], qualifiers, nil) }
            conflict[:columns] = conflict[:elements].map { |node| node[:name] }
          end
          conflict[:assignments] = rewrite_view_assignments(conflict[:assignments] || [], view, plan, qualifiers, label)
          conflict[:where] = rewrite_view_value(conflict[:where], plan[:columns], qualifiers, label)
          conflict[:predicate] = rewrite_view_value(conflict[:predicate], plan[:columns], qualifiers, nil)
          rewritten[:on_conflict] = conflict
        end
      elsif operation == :update
        rewritten[:assignments] = rewrite_view_assignments(statement[:assignments], view, plan, qualifiers, label)
      end
      if operation != :insert
        own_where = rewrite_view_value(statement[:where], plan[:columns], qualifiers, label)
        rewritten[:where] = conjunction(relabel_view_targets(plan[:predicate], label), own_where)
      end
      if statement[:returning]
        projections = statement[:returning].flat_map do |item|
          node = item[:expr]
          if node[:kind] == :star
            if node[:table] && !qualifiers.include?(node[:table])
              raise Error.new('42P01', 'unknown RETURNING relation')
            end
            view_schema(view).map { |column| { expr: { kind: :column, name: column[:name], table: node[:table] }, alias: column[:name] } }
          else
            [item.merge(alias: item[:alias] || expression_name(node))]
          end
        end
        rewritten[:returning] = projections.map do |item|
          item.merge(expr: rewrite_view_value(item[:expr], plan[:columns], qualifiers, label))
        end
      end
      previous_checks = @view_write_checks
      @view_write_checks = plan[:checks].map { |node| compile_filter(relabel_view_targets(node, label), table_columns(table(plan[:table]), label)) }
      yield rewritten
    ensure
      @view_write_checks = previous_checks
    end

    def view_checks_pending?
      @view_write_checks && !@view_write_checks.empty?
    end

    def validate_view_candidate(_table, row)
      (@view_write_checks || []).each do |predicate|
        raise Error.new('44000', 'new row violates view CHECK OPTION') unless evaluate(predicate, row) == true
      end
    end

    def view_write_plan(view, operation, seen = [])
      raise Error.new('42P17', 'recursive view is not updatable') if seen.include?(view[:name])
      raise Error.new('55000', 'view is not automatically updatable') unless updatable_view_shape?(view)
      query = view[:query]
      source = query[:from].first
      if (inner = @views[source[:name]])
        child = view_write_plan(inner, operation, seen + [view[:name]])
        input_names = view_schema(inner).map { |column| column[:name] }
      else
        base = table(source[:name])
        input_names = base[:columns].map { |column| column[:name] }
        child = { table: source[:name], columns: input_names.to_h { |name| [name, { kind: :column, name: name, view_target: true }] },
                  writable: input_names.to_h { |name| [name, name] }, predicate: nil, checks: [], defaults: {} }
      end
      aliases = source[:columns] || []
      input_map = input_names.each_with_index.to_h { |name, index| [aliases[index] || name, child[:columns][name]] }
      input_writable = input_names.each_with_index.to_h { |name, index| [aliases[index] || name, child[:writable][name]] }
      input_defaults = input_names.each_with_index.to_h do |name, index|
        [aliases[index] || name, inner && inner[:defaults]&.[](name) || child[:defaults][name]]
      end
      label = source[:alias] || source[:name]
      columns, writable, defaults = {}, {}, {}
      view_schema(view).each_with_index do |column, index|
        expression = query[:columns][index][:expr]
        columns[column[:name]] = rewrite_view_value(expression, input_map, [label], nil)
        if expression[:kind] == :column && (!expression[:table] || expression[:table] == label)
          writable[column[:name]] = input_writable[expression[:name]] if input_writable[expression[:name]]
          inherited = input_defaults[expression[:name]]
          defaults[column[:name]] = inherited if inherited
        end
      end
      raise Error.new('55000', 'view has no updatable columns') if operation != :delete && writable.empty?
      predicate = rewrite_view_value(query[:where], input_map, [label], nil)
      checks = child[:checks].dup
      if view[:check_option]
        condition = view[:check_option] == :local ? predicate : conjunction(child[:predicate], predicate)
        checks << condition if condition
      end
      { table: child[:table], columns: columns, writable: writable,
        predicate: conjunction(child[:predicate], predicate), checks: checks, defaults: defaults }
    end

    def writable_view_column(plan, name)
      raise Error.new('42703', "unknown view column #{name}") unless plan[:columns].key?(name)
      plan[:writable][name] || raise(Error.new('0A000', "view column #{name} is not updatable"))
    end

    def rewrite_view_assignments(assignments, view, plan, qualifiers, label)
      assignments.map do |assignment|
        names = assignment[:columns] || [assignment[:column]]
        targets = names.map { |name| writable_view_column(plan, name) }
        node = assignment[:expr]
        if node[:kind] == :default
          node = view[:defaults]&.[](names.first) || { kind: :literal, type: 'unknown', value: nil }
        else
          node = rewrite_view_value(node, plan[:columns], qualifiers, label)
        end
        assignment.merge(column: targets.first, columns: assignment[:columns] && targets, expr: node)
      end
    end

    def rewrite_view_value(value, mapping, qualifiers, current_label, local_names = [])
      case value
      when Array then value.map { |child| rewrite_view_value(child, mapping, qualifiers, current_label, local_names) }
      when Hash
        if value[:kind] == :column
          qualifier = value[:table]
          return value if qualifier ? !qualifiers.include?(qualifier) : local_names.include?(value[:name])
          replacement = mapping[value[:name]]
          raise Error.new('42703', "unknown view column #{value[:name]}") unless replacement
          replacement = Marshal.load(Marshal.dump(replacement))
          version = current_label ? qualifier || current_label : qualifier
          visit_view_nodes(replacement) { |node| node[:table] = version if node[:kind] == :column && node[:view_target] }
          replacement
        elsif %i[select values set_operation].include?(value[:kind])
          sources = (value[:from] || []) + (value[:joins] || []).map { |join| join[:relation] }
          analyzer = ParameterBindings.new(self, value)
          local_columns = sources.flat_map { |source| analyzer.relation_columns(source) }
          local_qualifiers = sources.map { |source| source[:alias] || source[:name] }
          active_qualifiers = qualifiers - local_qualifiers
          value.transform_values do |child|
            rewrite_view_value(child, mapping, active_qualifiers, current_label, local_names + local_columns.map { |column| column[:name] })
          end
        else
          value.transform_values { |child| rewrite_view_value(child, mapping, qualifiers, current_label, local_names) }
        end
      else value
      end
    end

    def conjunction(left, right)
      return left || right unless left && right
      { kind: :view_filter, left: left, right: right }
    end

    def relabel_view_targets(value, label)
      return nil unless value
      copy = Marshal.load(Marshal.dump(value))
      visit_view_nodes(copy) { |node| node[:table] = label if node[:kind] == :column && node[:view_target] }
      copy
    end

    def relation_name_taken?(name)
      @tables.key?(name) || @views.key?(name) || @sequences.key?(name) || @tables.values.any? { |table| (table[:indexes] || []).any? { |index| index[:name] == name } }
    end

    def view_schema(view)
      return view[:schema] if view[:schema]
      view_query_columns(view[:query]).each_with_index.map { |column, index| column.merge(name: view[:columns]&.at(index) || column[:name]) }
    end

    def view_query_columns(query, ctes = {}, outer = [])
      analyzer = ParameterBindings.new(self, query)
      analyzer.instance_variable_set(:@ctes, ctes)
      columns = analyzer.query_columns(query, outer)
      if query[:kind] == :select && !query[:with]
        sources = (query[:from] || []) + (query[:joins] || []).map { |join| join[:relation] }
        scope = sources.flat_map { |source| analyzer.relation_columns(source, outer) }
        (query[:columns] || []).each_with_index do |item, index|
          expression = item[:expr]
          modifier = case expression[:kind]
                     when :cast then expression[:type_mod]
                     when :column
                       scope.find { |column| column[:name] == expression[:name] && (!expression[:table] || column[:table] == expression[:table]) }&.dig(:type_mod)
                     end
          columns[index] = columns[index].merge(type_mod: modifier) if columns[index]
        end
      end
      columns
    end

    def view_dependencies(view)
      return view[:dependencies] if view[:dependencies]
      dependencies = {}
      freeze_view_query(view[:query], {}, [], dependencies)
      dependencies
    end

    def view_catalog_dependencies(query, schema)
      sequences, types = [], []
      add_type = lambda do |type|
        base = Types.base(type).delete_suffix('[]')
        if @types.key?(base) && !types.include?(base)
          types << base
          @types[base].each { |field| add_type.call(field[:type]) }
        end
      end
      schema.each { |column| add_type.call(column[:type]) }
      visit_view_nodes(query) do |node|
        add_type.call(node[:type]) if node[:kind] == :cast
        if node[:kind] == :call && %w[nextval currval setval].include?(node[:name]) && node[:args]&.first&.dig(:kind) == :literal
          name = node[:args].first[:value]
          sequences << name unless sequences.include?(name)
        end
      end
      [sequences, types]
    end

    def drop_catalog_view_dependencies(key, names, cascade)
      dependents = @views.select do |_, view|
        refs = view[key] || view_catalog_dependencies(view[:query], view_schema(view))[key == :sequence_dependencies ? 0 : 1]
        (refs & names).any?
      end.keys
      return if dependents.empty?
      raise Error.new('2BP01', 'view depends on catalog object') unless cascade
      drop_dependent_views(dependents, true, except: dependents)
      dependents.each { |name| @views.delete(name) }
    end

    def freeze_view_query(query, ctes, outer, dependencies)
      query = query.dup
      ctes = ctes.dup
      if query[:with]
        query[:with] = query[:with].map do |cte|
          body = cte[:query]
          if query[:recursive] && body[:kind] == :set_operation
            seed = view_query_columns(body[:left], ctes, outer)
            ctes[cte[:name]] = seed.each_with_index.map { |column, index| column.merge(name: cte[:columns]&.at(index) || column[:name]) }
          end
          frozen_body = freeze_view_query(body, ctes, outer, dependencies)
          ctes[cte[:name]] = view_query_columns(frozen_body, ctes, outer).each_with_index.map do |column, index|
            column.merge(name: cte[:columns]&.at(index) || column[:name])
          end
          cte.merge(query: frozen_body)
        end
      end
      case query[:kind]
      when :select
        scope = []
        query[:from] = (query[:from] || []).map do |source|
          frozen_source, columns = freeze_view_source(source, ctes, scope + outer, dependencies)
          scope.concat(columns)
          frozen_source
        end
        query[:joins] = (query[:joins] || []).map do |join|
          source, columns = freeze_view_source(join[:relation], ctes, scope + outer, dependencies)
          using = join[:natural] ? (scope.map { |column| column[:name] } & columns.map { |column| column[:name] }) : (join[:using] || [])
          using.each do |name|
            (scope + columns).select { |column| column[:name] == name }.each { |column| record_view_dependency(column, dependencies) }
          end
          columns = columns.map { |column| using.include?(column[:name]) ? column.merge(qualified_only: true) : column }
          scope.concat(columns)
          join.merge(relation: source, on: freeze_view_expression(join[:on], ctes, scope + outer, dependencies))
        end
        query[:columns] = query[:columns].flat_map do |item|
          if item[:expr][:kind] == :star
            expand_stars([item], scope).map do |expanded|
              expanded.merge(expr: expanded[:expr].merge(quoted: true, table_quoted: true), generated_star: true)
            end
          else
            [item]
          end
        end
        query.each do |key, value|
          next if %i[from joins with].include?(key)
          query[key] = freeze_view_expression(value, ctes, scope + outer, dependencies)
        end
      when :set_operation
        query[:left] = freeze_view_query(query[:left], ctes, outer, dependencies)
        query[:right] = freeze_view_query(query[:right], ctes, outer, dependencies)
      when :values
        query[:rows] = freeze_view_expression(query[:rows], ctes, outer, dependencies)
      end
      query
    end

    def freeze_view_source(source, ctes, outer, dependencies)
      label = source[:alias] || source[:name]
      columns = case source[:kind]
                when :table
                  if ctes.key?(source[:name])
                    ctes[source[:name]]
                  elsif (view = @views[source[:name]])
                    source = source.merge(view_bound_relation: true)
                    dependencies[source[:name]] ||= []
                    view_schema(view)
                  else
                    source = source.merge(view_bound_relation: true)
                    table = table(source[:name])
                    dependencies[source[:name]] ||= []
                    table_columns(table).map { |column| column.merge(view_relation: source[:name], view_column: column[:name]) }
                  end
                when :subquery
                  query = freeze_view_query(source[:query], ctes, source[:lateral] ? outer : [], dependencies)
                  source = source.merge(query: query)
                  view_query_columns(query, ctes, source[:lateral] ? outer : [])
                when :join_group
                  inner = { kind: :select, columns: [{ expr: { kind: :star } }], from: source[:from], joins: source[:joins] }
                  frozen = freeze_view_query(inner, ctes, outer, dependencies)
                  source = source.merge(from: frozen[:from], joins: frozen[:joins])
                  view_query_columns(frozen, ctes, outer)
                else
                  analyzer = ParameterBindings.new(self, source)
                  analyzer.instance_variable_set(:@ctes, ctes)
                  analyzer.relation_columns(source, outer)
                end
      names = source[:columns] || []
      raise Error.new('42601', 'too many relation column aliases') if names.length > columns.length
      [source, columns.each_with_index.map { |column, index| column.merge(table: label || column[:table], name: names[index] || column[:name]) }]
    end

    def freeze_view_expression(value, ctes, scope, dependencies)
      case value
      when Array then value.map { |item| freeze_view_expression(item, ctes, scope, dependencies) }
      when Hash
        return freeze_view_query(value, ctes, scope, dependencies) if %i[select values set_operation].include?(value[:kind])
        if value[:kind] == :column
          matches = scope.select { |column| column[:name] == value[:name] && (value[:table] ? column[:table] == value[:table] : !column[:qualified_only]) }
          matches.each { |column| record_view_dependency(column, dependencies) }
        elsif value[:kind] == :call && %w[nextval currval setval].include?(value[:name]) && value[:args]&.first&.dig(:kind) == :literal
          name = value[:args].first[:value]
          raise Error.new('42P01', 'sequence does not exist') unless @sequences.key?(name)
        end
        value.transform_values { |child| freeze_view_expression(child, ctes, scope, dependencies) }
      else value
      end
    end

    def record_view_dependency(column, dependencies)
      return unless column[:view_relation]
      names = (dependencies[column[:view_relation]] ||= [])
      names << column[:view_column] unless names.include?(column[:view_column])
    end

    def visit_view_nodes(value, &block)
      case value
      when Array then value.each { |child| visit_view_nodes(child, &block) }
      when Hash
        yield value
        value.each_value { |child| visit_view_nodes(child, &block) }
      end
    end

    def validate_view_options(options)
      options.each do |key, value|
        if key.to_s == 'check_option'
          raise Error.new('22023', 'invalid CHECK OPTION') unless %w[local cascaded].include?(value.to_s.downcase)
        else
          raise Error.new('22023', 'unknown view option') unless %w[security_barrier security_invoker].include?(key.to_s)
          text = value.to_s.downcase
          choices = %w[true false yes no on off 1 0].select { |choice| choice.start_with?(text) }
          raise Error.new('22023', 'view option requires boolean') if text.empty? || choices.length != 1
        end
      end
    end

    def view_reaches?(names, target, seen = {})
      names.any? do |name|
        next true if name == target
        next false unless @views[name] && !seen[name]
        seen[name] = true
        view_reaches?(view_dependencies(@views[name]).keys, target, seen)
      end
    end

    def drop_dependent_views(names, cascade, except: [])
      remaining = @views.keys - except
      loop do
        found = remaining.select { |name| (view_dependencies(@views[name]).keys & names).any? }
        break if found.empty?
        raise Error.new('2BP01', 'view depends on relation') unless cascade
        found.each { |name| @views.delete(name) }
        remaining -= found
        names += found
      end
    end

    def rewrite_view_relation(old_name, new_name)
      @views.transform_values! do |view|
        next view unless view_dependencies(view).key?(old_name)
        query = Marshal.load(Marshal.dump(view[:query]))
        visit_view_nodes(query) do |node|
            if node[:kind] == :table && node[:name] == old_name && node[:view_bound_relation]
              node[:name] = new_name
              node[:quoted] = true
              unless node[:alias]
                node[:alias] = old_name
                node[:alias_quoted] = true
                node[:alias_explicit] = true
              end
          end
        end
        deps = view_dependencies(view).dup
        deps[new_name] = deps.delete(old_name)
        view.merge(query: query, dependencies: deps)
      end
    end

    def rewrite_view_column(relation, old_name, new_name)
      @views.transform_values! do |view|
        next view unless view_dependencies(view).key?(relation)
        query = rewrite_bound_view_column(view[:query], relation, old_name, new_name)
        deps = view_dependencies(view).transform_values(&:dup)
        deps[relation].map! { |name| name == old_name ? new_name : name }
        view.merge(query: query, dependencies: deps)
      end
    end

    def rewrite_bound_view_column(query, relation, old_name, new_name)
      return query.map { |item| rewrite_bound_view_column(item, relation, old_name, new_name) } if query.is_a?(Array)
      return query unless query.is_a?(Hash)
      if query[:kind] == :table && query[:name] == relation && query[:view_bound_relation]
        columns = @tables[relation] ? @tables[relation][:columns] : view_schema(@views[relation])
        aliases = query[:columns] || columns.map { |column| column[:name] == new_name ? old_name : column[:name] }
        changed = query.merge(columns: aliases, columns_quoted: Array.new(aliases.length, true))
        changed.merge!(alias: query[:name], alias_quoted: true, alias_explicit: true) unless query[:alias]
        return changed
      end
      query.transform_values { |child| rewrite_bound_view_column(child, relation, old_name, new_name) }
    end

    # File open keeps stored views parse-only. Binding is needed only when a DDL
    # edit must distinguish catalog relations from CTEs with the same spelling.
    def bind_imported_view_dependents(relation)
      @views.transform_values! do |view|
        next view unless view[:storage_view] && view_dependencies(view).key?(relation)
        query = freeze_view_query(view[:query], {}, [], {})
        view.merge(query: query)
      end
    end

    def updatable_view_shape?(view)
      query = view[:query]
      query[:kind] == :select && !query[:with] && !query[:distinct] && !query[:distinct_on] &&
        (query[:group_by] || []).empty? && !query[:having] && !query[:limit] && !query[:offset] &&
        (query[:from] || []).length == 1 && query[:from].first[:kind] == :table && (query[:joins] || []).empty? &&
        !(query[:columns] || []).any? { |item| contains_aggregate?(item[:expr]) || !window_nodes(item[:expr]).empty? }
    end
  end
  Engine.prepend(Views)
end
