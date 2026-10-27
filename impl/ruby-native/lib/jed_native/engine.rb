# frozen_string_literal: true

module JedNative
  # A resolved expression contains Ruby values and column offsets. Runtime
  # compilation uses trusted templates; SQL text/values never become Ruby source
  # and names never become method dispatch on a host object.
  Expression = Struct.new(:kind, :type, :value, :args, :extra, keyword_init: true)
  Relation = Struct.new(:columns, :rows, keyword_init: true)

  class Engine
    attr_reader :tables, :cost, :max_scalar_bytes
    attr_accessor :max_cost
    attr_accessor :lifetime_max_cost
    attr_reader :lifetime_cost
    def max_scalar_bytes=(amount)
      @max_scalar_bytes = amount.positive? ? amount : 64 * 1024 * 1024
    end

    def initialize(environment: nil, max_cost: 0, lifetime_max_cost: 0, max_scalar_bytes: 64 * 1024 * 1024, **options)
      raise Error.new('0A000', "unsupported settings: #{options.keys.join(', ')}") unless options.empty?
      @environment = environment
      @tables, @views, @types, @sequences = {}, {}, {}, {}
      @cost, @max_cost, @max_scalar_bytes = 0, max_cost, max_scalar_bytes
      self.max_scalar_bytes = max_scalar_bytes
      @lifetime_cost, @lifetime_max_cost = 0, lifetime_max_cost
      @params, @ctes, @outer = [], {}, []
      @transaction, @failed, @dirty = nil, false, false
      @savepoints = []
      @closed = false
    end

    def run(statement, params = [])
      raise Error.new('55000', 'database is closed') if @closed
      if @lifetime_max_cost.positive? && @lifetime_cost >= @lifetime_max_cost
        raise Error.new('54P02', 'session cost limit exceeded')
      end
      @cost, @scalar_bytes, @params = 0, 0, params
      @statement_clock, @uuid_counter = nil, 0
      @foreign_key_checks = {}
      raise Error.new('25001', 'already in transaction') if statement[:kind] == :begin && @transaction
      if @failed && !%i[rollback rollback_to commit].include?(statement[:kind])
        raise Error.new('25P02', 'transaction is aborted')
      end
      before = snapshot
      previous_dirty = @dirty
      begin
        if @read_only && !(statement[:kind] == :explain && !statement[:analyze]) && !temporary_statement?(statement) && !%i[select values set_operation setop with commit rollback rollback_to savepoint release].include?(statement[:kind])
          raise Error.new('25006', 'read-only transaction')
        end
        result = dispatch(statement)
        flush_foreign_keys
        # A frozen reader already captured prepared catalog/index metadata.
        # Eager reads can also reuse a completed refresh, within the same
        # catalog and immutable host-function environment. Changed catalogs,
        # transactions and statements that dirty them retain the write path.
        skip_refresh = @native_cursor && !@dirty
        if !skip_refresh && !@dirty && !@transaction && @native_refreshed_tables.equal?(@tables) &&
           @native_refreshed_types.equal?(@types) && @native_refreshed_environment.equal?(@environment)
          skip_refresh = case statement[:kind]
                         when :select, :values, :set_operation, :setop, :with then true
                         else false
                         end
        end
        refresh_primary_indexes unless skip_refresh
        persist if @dirty && !@transaction
        unless skip_refresh
          @native_refreshed_tables, @native_refreshed_types, @native_refreshed_environment = @tables, @types, @environment
        end
        Result.new(columns: result.columns, column_types: result.column_types,
                   rows: result.rows, rows_affected: result.rows_affected, cost: @cost)
      rescue StandardError
        restore(before)
        @dirty = previous_dirty
        @failed = true if @transaction
        raise
      end
    end

    def charge(amount = 1)
      @cost += amount
      @lifetime_cost += amount
      raise Error.new('54P01', 'statement cost limit exceeded') if @max_cost.positive? && @cost >= @max_cost
      raise Error.new('54P02', 'session cost limit exceeded') if @lifetime_max_cost.positive? && @lifetime_cost >= @lifetime_max_cost
    end

    def consume_scalar_bytes(amount)
      @scalar_bytes += amount
      if @max_scalar_bytes.positive? && @scalar_bytes > @max_scalar_bytes
        raise Error.new('54P04', 'scalar allocation limit exceeded')
      end
    end

    def dispatch(stmt)
      if stmt[:with] && %i[insert update delete merge].include?(stmt[:kind])
        previous_ctes = @ctes
        begin
          @ctes = @ctes.dup
          materialize_ctes(stmt)
          return dispatch(stmt.reject { |key, _| key == :with })
        ensure
          @ctes = previous_ctes
        end
      end
      case stmt[:kind]
      when :select, :values, :set_operation, :setop, :with
        rel = select(stmt)
        Result.new(columns: rel.columns.map { |c| c[:name] }, column_types: rel.columns.map { |c| c[:type] }, rows: rel.rows)
      when :create_table then create_table(stmt)
      when :drop_table then drop_table(stmt)
      when :insert then insert(stmt)
      when :update then update(stmt)
      when :delete then delete(stmt)
      when :merge then merge(stmt)
      when :truncate then truncate(stmt)
      when :create_index then create_index(stmt)
      when :drop_index then drop_index(stmt)
      when :create_view then create_view(stmt)
      when :drop_view then drop_view(stmt)
      when :begin
        raise Error.new('25001', 'already in transaction') if @transaction
        @transaction = snapshot
        @dirty = false
        Result.new
      when :commit
        if @failed
          restore(@transaction) if @transaction
          @dirty = false
        else
          persist if @dirty
        end
        @transaction, @failed, @savepoints = nil, false, []
        Result.new
      when :rollback
        restore(@transaction) if @transaction
        @transaction, @failed, @dirty, @savepoints = nil, false, false, []
        Result.new
      when :savepoint
        raise Error.new('25P01', 'SAVEPOINT outside transaction') unless @transaction
        @savepoints << [stmt[:name], snapshot, @dirty]
        Result.new
      when :rollback_to, :release
        raise Error.new('25P01', 'savepoint outside transaction') unless @transaction
        index = @savepoints.rindex { |entry| entry[0] == stmt[:name] }
        raise Error.new('3B001', 'unknown savepoint') unless index
        if stmt[:kind] == :rollback_to
          restore(@savepoints[index][1])
          @dirty, @failed = @savepoints[index][2], false
          @savepoints = @savepoints.take(index + 1)
        else
          @savepoints = @savepoints.take(index)
        end
        Result.new
      when :create_type
        name = stmt[:name]
        raise Error.new('42710', "type #{name} exists") if @types.key?(name)
        @types = @types.merge(name => stmt[:fields] || stmt[:columns])
        @dirty = true
        Result.new
      when :drop_type
        raise Error.new('42704', 'type does not exist') unless @types.key?(stmt[:name]) || stmt[:if_exists]
        @types = @types.reject { |name, _| name == stmt[:name] }
        @dirty = true
        Result.new
      when :create_sequence then create_sequence(stmt)
      when :drop_sequence
        raise Error.new('42P01', 'sequence does not exist') unless @sequences.key?(stmt[:name]) || stmt[:if_exists]
        @sequences = @sequences.reject { |name, _| name == stmt[:name] }
        @dirty = true
        Result.new
      else raise Error.new('0A000', "statement #{stmt[:kind]} is not implemented")
      end
    end

    def snapshot = [@tables.dup, @views.dup, @types.dup, @sequences.transform_values(&:dup)]
    def transaction_open? = !@transaction.nil?
    def restore(state)
      @tables, @views, @types = state.take(3).map(&:dup)
      @sequences = state[3].transform_values(&:dup)
    end
    def persist = (@dirty = false)
    def close = (@closed = true)

    def table(name)
      stored = @tables[name] || @tables[name.to_s.sub(/\Amain\./, '')]
      return stored if stored
      raise Error.new('42809', "relation #{name} is not a table") if @views.key?(name) || @sequences.key?(name)
      raise Error.new('42P01', "relation #{name} does not exist")
    end

    def table_columns(table, name = table[:name])
      table[:columns].each_with_index.map { |c, i| { name: c[:name], table: name, type: c[:type], source_ordinal: i } }
    end

    def create_table(stmt)
      name = stmt[:name]
      if relation_exists?(name)
        return Result.new if stmt[:if_not_exists]
        raise Error.new('42P07', "relation #{name} already exists")
      end
      if stmt[:query]
        rel = select(stmt[:query])
        raise Error.new('42P16', 'anonymous record columns cannot be stored') if rel.columns.any? { |c| c[:type] == 'record' || c[:type] == 'record[]' }
        cols = rel.columns.map { |c| { name: c[:name], type: c[:type], nullable: true } }
        rows = stmt[:with_no_data] ? [] : rel.rows
      else
        cols = stmt.fetch(:columns).map do |column|
          c = column.dup
          c[:type] = Types.normalize(c[:type])
          validate_type(c[:type], c[:type_mod])
          raise Error.new('0A000', 'identity columns are not implemented') if c[:identity]
          c[:nullable] = false if c[:primary_key]
          c
        end
        rows = []
      end
      raise Error.new('42701', 'duplicate column') unless cols.map { |c| c[:name] }.uniq.length == cols.length
      constraints = (stmt[:constraints] || []).dup
      cols.each do |c|
        constraints << { kind: :primary_key, columns: [c[:name]] } if c[:primary_key]
        constraints << { kind: :unique, columns: [c[:name]], name: c[:unique_name] } if c[:unique]
        (c[:checks] || []).each { |expr| constraints << (expr[:kind] == :check ? expr : { kind: :check, expr: expr }) }
        constraints << c[:references].merge(kind: :foreign_key, columns: [c[:name]]) if c[:references]
      end
      constraints = Marshal.load(Marshal.dump(constraints))
      raise Error.new('42P16', 'multiple primary keys') if constraints.count { |c| c[:kind] == :primary_key } > 1
      constraints.each do |constraint|
        (constraint[:columns] || []).each do |column|
          c = cols.find { |candidate| candidate[:name] == column }
          raise Error.new('42703', "unknown column #{column}") unless c
          c[:nullable] = false if constraint[:kind] == :primary_key
        end
      end
      cols.each do |column|
        next unless column[:default]
        expression = compile(column[:default], [])
        assign(expression.value, column.merge(nullable: true), expression) if expression.kind == :literal
      end
      t = { name: name, columns: cols, rows: rows, rowids: (1..rows.length).to_a, next_rowid: rows.length + 1,
            constraints: constraints, indexes: [], temporary: stmt[:temporary] }
      validate_definition(t)
      @tables[name] = t
      @dirty = true
      Result.new
    end

    def drop_table(stmt)
      names = stmt[:names] || [stmt[:name]]
      names.each do |name|
        raise Error.new('42P01', "relation #{name} does not exist") unless @tables.key?(name) || stmt[:if_exists]
        @tables.each do |child_name, child|
          next if names.include?(child_name)
          dependencies = child[:constraints].select { |c| c[:kind] == :foreign_key && c[:table] == name }
          next if dependencies.empty?
          raise Error.new('2BP01', 'foreign key depends on table') unless stmt[:cascade]
          @tables[child_name] = child.merge(constraints: child[:constraints] - dependencies)
        end
        @tables.delete(name)
      end
      @dirty = true
      Result.new
    end

    def create_view(stmt)
      raise Error.new('42P07', 'relation already exists') if @tables.key?(stmt[:name]) || (@views.key?(stmt[:name]) && !stmt[:replace])
      select(stmt[:query])
      @views[stmt[:name]] = stmt
      @dirty = true
      Result.new
    end

    def drop_view(stmt)
      raise Error.new('42P01', 'view does not exist') unless @views.key?(stmt[:name]) || stmt[:if_exists]
      @views.delete(stmt[:name])
      @dirty = true
      Result.new
    end

    def create_index(stmt)
      t = table(stmt[:table])
      unless stmt[:name]
        base = "#{t[:name]}_#{(stmt[:elements] || []).map { |e| e.dig(:expr, :name) || 'expr' }.join('_')}_idx"
        name, suffix = base, 0
        while relation_exists?(name)
          suffix += 1
          name = "#{base}#{suffix}"
        end
        stmt = stmt.merge(name: name)
      end
      if relation_exists?(stmt[:name])
        return Result.new if stmt[:if_not_exists]
        raise Error.new('42P07', 'index exists')
      end
      raise Error.new('0A000', 'only btree indexes are implemented') if stmt[:method] && stmt[:method] != 'btree'
      charge(full_scan_pages(t) + t[:rows].length) unless t[:rows].empty?
      scope = table_columns(t)
      (stmt[:elements] || []).each { |element| validate_index_expression(element[:expr], scope) }
      if stmt[:where]
        validate_index_expression(stmt[:where], scope)
        compile_filter(stmt[:where], scope)
      end
      updated = t.merge(indexes: t[:indexes] + [stmt])
      validate_rows(updated, updated[:rows])
      @tables[t[:name]] = updated
      @dirty = true
      Result.new
    end

    def drop_index(stmt)
      if @tables.key?(stmt[:name]) || @views.key?(stmt[:name]) || @sequences.key?(stmt[:name])
        raise Error.new('42809', 'relation is not an index')
      end
      found = false
      @tables.each do |name, t|
        indexes = t[:indexes].reject { |i| i[:name] == stmt[:name] }
        if indexes.length != t[:indexes].length
          found = true
          @tables[name] = t.merge(indexes: indexes)
        end
      end
      raise Error.new('42704', 'index does not exist') unless found || stmt[:if_exists]
      @dirty ||= found
      Result.new
    end

    def truncate(stmt)
      (stmt[:tables] || stmt[:names] || [stmt[:table] || stmt[:name]]).each do |name|
        t = table(name)
        @tables[t[:name]] = t.merge(rows: [], rowids: [], next_rowid: 1)
      end
      @dirty = true
      Result.new
    end

    def assign(value, column, source = nil)
      target = column[:type]
      source_type = source&.type || Types.infer(value)
      literal = source && %i[literal param].include?(source.kind)
      numeric = %w[i16 i32 i64 decimal f32 f64]
      unless source_type == target || literal || source_type == 'record' || (source_type.end_with?('[]') && target.end_with?('[]')) || (numeric.include?(source_type) && numeric.include?(target)) || %w[null unknown].include?(source_type) || source_type == 'jsonb' && target == 'json'
        raise Error.new('42804', "cannot assign #{source_type} to #{target}")
      end
      if value.nil?
        raise Error.new('23502', "NULL in #{column[:name]}") if column[:nullable] == false
        return nil
      end
      result = cast_value(value, target, source_type: source_type)
      mod = column[:type_mod] || column[:typmod]
      if mod && target == 'decimal' && result.respond_to?(:round)
        precision, scale = mod.is_a?(Hash) ? [mod[:precision], mod[:scale]] : Array(mod)
        result = result.round(scale || 0)
        raise Error.new('22003', 'numeric field overflow') if result.abs >= 10**(precision - (scale || 0))
      elsif mod && target == 'text'
        max = mod.is_a?(Hash) ? mod[:precision] : Array(mod).first
        raise Error.new('22001', 'string too long') if result.rstrip.length > max
        result = result[0, max]
      end
      result
    end

    def validate_type(type, modifier = nil)
      scalar = %w[i16 i32 i64 f32 f64 decimal boolean text bytea uuid date timestamp timestamptz interval json jsonb jsonpath]
      if type.end_with?('[]')
        validate_type(type[0...-2])
      elsif !scalar.include?(type) && !Types::RANGES.key?(type) && !@types.key?(type)
        raise Error.new('42704', "type #{type} does not exist")
      end
      return unless modifier
      precision = modifier.is_a?(Hash) ? modifier[:precision] : modifier[0]
      scale = modifier.is_a?(Hash) ? modifier[:scale] : modifier[1]
      case type
      when 'decimal'
        raise Error.new('22023', 'invalid decimal precision') unless precision && precision.between?(1, 1000) && (scale || 0).between?(0, precision)
      when 'text'
        raise Error.new('22023', 'invalid text length') unless precision && precision.between?(1, 10_485_760)
      when 'timestamp', 'timestamptz', 'interval'
        raise Error.new('22023', 'invalid temporal precision') if precision && !precision.between?(0, 6)
      else raise Error.new('42601', "#{type} does not accept a type modifier")
      end
    end

    def validate_rows(t, rows)
      cols = table_columns(t)
      constraints = t[:constraints]
      constraints.each do |constraint|
        case constraint[:kind]
        when :primary_key, :unique
          indices = constraint[:columns].map { |name| cols.index { |c| c[:name] == name } }
          seen = {}
          rows.each do |row|
            key = indices.map { |i| row[i] }
            if key.any?(&:nil?)
              raise Error.new('23502', 'NULL primary key') if constraint[:kind] == :primary_key
              next
            end
            raise Error.new('23505', 'duplicate key') if seen.key?(key)
            seen[key] = true
          end
        when :check
          expr = compile(constraint[:expr], cols)
          raise Error.new('42804', 'CHECK must be boolean') unless %w[boolean unknown].include?(expr.type)
          rows.each { |row| raise Error.new('23514', 'check constraint violation') if evaluate(expr, row) == false }
        when :foreign_key
          unless @checking_foreign_keys
            @foreign_key_checks[t[:name]] = true
            next
          end
          parent_name = constraint[:table] || constraint[:ref_table] || constraint[:references]&.dig(:table)
          parent = parent_name == t[:name] ? t.merge(rows: rows) : table(parent_name)
          names = constraint[:ref_columns] || constraint[:reference_columns] || constraint[:references]&.dig(:columns)
          names ||= parent[:constraints].find { |c| c[:kind] == :primary_key }&.dig(:columns)
          raise Error.new('42830', 'no referenced key') unless names
          indices = constraint[:columns].map { |n| cols.index { |c| c[:name] == n } }
          parent_indices = names.map { |n| parent[:columns].index { |c| c[:name] == n } }
          keys = parent[:rows].to_h { |r| [parent_indices.map { |i| r[i] }, true] }
          rows.each do |row|
            key = indices.map { |i| row[i] }
            if key.any?(&:nil?)
              raise Error.new('23503', 'MATCH FULL does not allow mixed nulls') if constraint[:match] == :full && !key.all?(&:nil?)
              next
            end
            matches = keys.key?(key) || keys.each_key.any? { |candidate| key.zip(candidate).all? { |a, b| Types.compare(a, b).zero? } }
            raise Error.new('23503', 'foreign key violation') unless matches
          end
        when :exclude
          predicate = compile_filter(constraint[:where], cols)
          elements = constraint[:elements].map { |e| [compile(e[:expr], cols), e[:op]] }
          keys = rows.filter_map do |row|
            next if predicate && evaluate(predicate, row) != true
            elements.map { |e, _| evaluate(e, row) }
          end
          keys.each_with_index do |left, i|
            keys.take(i).each do |right|
              conflict = elements.each_index.all? { |j| Types.binary(elements[j][1], left[j], right[j], type: 'boolean') == true }
              raise Error.new('23P01', 'exclusion constraint violation') if conflict
            end
          end
        end
      end
      t[:indexes].each do |index|
        next unless index[:unique]
        elements = index[:elements] || index[:columns].map { |name| { expr: { kind: :column, name: name } } }
        expressions = elements.map { |e| compile(e[:expr], cols) }
        predicate = compile_filter(index[:where], cols)
        seen = {}
        rows.each do |row|
          next if predicate && evaluate(predicate, row) != true
          key = expressions.map { |e| evaluate(e, row) }
          next if key.any?(&:nil?)
          raise Error.new('23505', 'duplicate unique index key') if seen[key]
          seen[key] = true
        end
      end
    end

    def insert(stmt)
      raise Error.new('0A000', 'ON CONFLICT is not implemented') if stmt[:on_conflict]
      t = table(stmt[:table])
      names = stmt[:columns] || t[:columns].map { |c| c[:name] }
      raise Error.new('42701', 'duplicate target column') unless names.uniq.length == names.length
      indices = names.map do |name|
        t[:columns].index { |c| c[:name] == name } || raise(Error.new('42703', "unknown column #{name}"))
      end
      source = stmt[:query] ? select(stmt[:query]) : nil
      inputs = source ? source.rows : (stmt[:rows] || [[]])
      inserted = inputs.map do |input|
        raise Error.new('42601', 'INSERT column count mismatch') unless input.length == indices.length || stmt[:default_values]
        row = t[:columns].map do |column|
          column[:default] ? evaluate(compile(column[:default], []), []) : nil
        end
        input.each_with_index do |value, i|
          expr = source ? Expression.new(kind: :literal, type: source.columns[i][:type]) : compile(value, [])
          value = evaluate(expr, []) unless source
          row[indices[i]] = assign(value, t[:columns][indices[i]], expr)
        end
        row.each_with_index { |value, i| row[i] = assign(value, t[:columns][i]) }
        row
      end
      rows = t[:rows] + inserted
      validate_rows(t, rows)
      next_id = t[:next_rowid] || t[:rows].length + 1
      ids = Array.new(inserted.length) { |i| next_id + i }
      @tables[t[:name]] = t.merge(rows: rows, rowids: (t[:rowids] || []) + ids, next_rowid: next_id + inserted.length)
      @dirty ||= !inserted.empty?
      returning(stmt[:returning], t, inserted)
    end

    def update(stmt)
      raise Error.new('0A000', 'UPDATE FROM is not implemented') if stmt[:from] && !stmt[:from].empty?
      t = table(stmt[:table])
      cols = table_columns(t, stmt[:alias] || t[:name])
      predicate = compile_filter(stmt[:where], cols)
      assignments = stmt[:assignments].map do |a|
        i = t[:columns].index { |c| c[:name] == a[:column] }
        raise Error.new('42703', "unknown column #{a[:column]}") unless i
        [i, compile(a[:expr], cols)]
      end
      changed, changes = [], []
      charge unless t[:rows].empty?
      rows = t[:rows].map do |row|
        charge
        next row unless predicate.nil? || evaluate(predicate, row) == true
        new_row = row.dup
        assignments.each { |i, expr| new_row[i] = assign(evaluate(expr, row), t[:columns][i], expr) }
        changed << new_row
        changes << [row, new_row]
        new_row
      end
      validate_rows(t, rows)
      @tables[t[:name]] = t.merge(rows: rows)
      enforce_references(t, changes, :update)
      @dirty ||= !changed.empty?
      returning(stmt[:returning], t, changed)
    end

    def delete(stmt)
      raise Error.new('0A000', 'DELETE USING is not implemented') if stmt[:using] && !stmt[:using].empty?
      t = table(stmt[:table])
      cols = table_columns(t, stmt[:alias] || t[:name])
      predicate = compile_filter(stmt[:where], cols)
      deleted, kept, kept_ids = [], [], []
      charge unless t[:rows].empty?
      t[:rows].each_with_index do |row, index|
        charge
        if predicate.nil? || evaluate(predicate, row) == true
          deleted << row
        else
          kept << row
          kept_ids << t[:rowids]&.dig(index)
        end
      end
      @tables[t[:name]] = t.merge(rows: kept, rowids: kept_ids)
      enforce_references(t, deleted.map { |row| [row, nil] }, :delete)
      @dirty ||= !deleted.empty?
      returning(stmt[:returning], t, deleted)
    end

    def enforce_references(parent, changes, operation, visited = {})
      return if changes.empty?
      @tables.to_a.each do |name, child|
        child[:constraints].each do |foreign|
          child = @tables[name]
          next unless foreign[:kind] == :foreign_key && foreign[:table] == parent[:name]
          columns = foreign[:ref_columns] || parent[:constraints].find { |c| c[:kind] == :primary_key }&.dig(:columns)
          next unless columns
          parent_indices = columns.map { |n| parent[:columns].index { |c| c[:name] == n } }
          child_indices = foreign[:columns].map { |n| child[:columns].index { |c| c[:name] == n } }
          mapping = {}
          changes.each do |before, after|
            old_key = parent_indices.map { |i| before[i] }
            new_key = after && parent_indices.map { |i| after[i] }
            mapping[old_key] = new_key unless old_key == new_key
          end
          next if mapping.empty?
          action = foreign[operation == :delete ? :on_delete : :on_update] || :no_action
          if %i[no_action restrict].include?(action)
            @foreign_key_checks[name] = true
            next
          end
          paged = child[:rows].is_a?(PagedRows)
          rows = paged ? PendingRows.new(child) : []
          rowids, child_changes = paged ? rows.rowids : [], []
          child[:rows].each_with_index do |row, index|
            key = child_indices.map { |i| row[i] }
            matched_key = if !key.any?(&:nil?)
              mapping.key?(key) ? key : mapping.each_key.find { |candidate| key.zip(candidate).all? { |a, b| Types.compare(a, b).zero? } }
            end
            if matched_key
              token = [name, row.object_id, operation, foreign[:name]]
              next if visited[token]
              visited[token] = true
              if action == :cascade && operation == :delete
                child_changes << [row, nil]
                rows.replace(row, nil) if paged
                next
              end
              replacement = mutation_copy_row(child, row)
              child_indices.each_with_index do |i, j|
                replacement[i] = case action
                when :cascade then mapping[matched_key][j]
                when :set_null then nil
                when :set_default then child[:columns][i][:default] ? evaluate(compile(child[:columns][i][:default], []), []) : nil
                else raise Error.new('0A000', "foreign key action #{action}")
                end
                replacement[i] = assign(replacement[i], child[:columns][i])
              end
              child_changes << [row, replacement]
              rows.replace(row, replacement) if paged
              row = replacement
            end
            unless paged
              rows << row
              rowids << child[:rowids]&.dig(index)
            end
          end
          query_memory_stage_changes(child, child_changes)
          @tables[name] = child.merge(rows: rows, rowids: rowids)
          validate_rows(@tables[name], rows)
          enforce_references(child, child_changes, operation, visited)
        end
      end
    end

    def returning(projections, t, rows)
      return Result.new(rows_affected: rows.length) unless projections && !projections.empty?
      rel = project(Relation.new(columns: table_columns(t), rows: rows), projections)
      Result.new(columns: rel.columns.map { |c| c[:name] }, column_types: rel.columns.map { |c| c[:type] }, rows: rel.rows, rows_affected: rows.length)
    end

    def create_sequence(stmt)
      raise Error.new('42P07', 'sequence exists') if @sequences.key?(stmt[:name])
      options = stmt[:options] || stmt
      @sequences[stmt[:name]] = { value: options[:start] || 1, increment: options[:increment] || 1, called: false }
      @dirty = true
      Result.new
    end

    def sequence_value(name, operation, value = nil, called = true)
      sequence = @sequences[name] || raise(Error.new('42P01', "sequence #{name} does not exist"))
      case operation.to_s
      when 'nextval'
        charge
        sequence[:value] += sequence[:increment] if sequence[:called]
        sequence[:called] = true
        @last_sequence = name
        @dirty = true
      when 'currval'
        raise Error.new('55000', 'sequence has not been used') unless sequence[:called]
      when 'setval'
        sequence[:value], sequence[:called] = value, called
        @dirty = true
      end
      sequence[:value]
    end

    def materialize_ctes(stmt)
      (stmt[:with] || stmt[:ctes] || []).each do |cte|
        raise Error.new('0A000', 'recursive SEARCH/CYCLE is not implemented') if cte[:search] || cte[:cycle]
        body = cte[:query]
        if stmt[:recursive] && %i[set_operation setop].include?(body[:kind]) && body[:op] == 'union'
          rel = select(body[:left])
          rel.columns = rel.columns.zip(cte[:columns]).map { |c, n| c.merge(name: n || c[:name]) } if cte[:columns]
          all_rows = rel.rows.dup
          seen = all_rows.to_h { |r| [r, true] }
          until rel.rows.empty?
            @ctes[cte[:name]] = rel
            next_rel = select(body[:right])
            next_rows = next_rel.rows
            next_rows = next_rows.reject { |r| seen[r] }.uniq unless body[:all]
            next_rows.each { |r| seen[r] = true } unless body[:all]
            all_rows.concat(next_rows)
            rel = Relation.new(columns: rel.columns, rows: next_rows)
          end
          rel = Relation.new(columns: rel.columns, rows: all_rows)
        elsif %i[insert update delete merge].include?(body[:kind])
          result = dispatch(body)
          rel = Relation.new(columns: result.columns.zip(result.column_types).map { |n, t| { name: n, type: t } }, rows: result.rows)
        else
          rel = select(body)
        end
        if cte[:columns]
          rel.columns = rel.columns.zip(cte[:columns]).map { |c, name| c.merge(name: name || c[:name]) }
        end
        @ctes[cte[:name]] = rel
      end
    end

    def select(stmt)
      old_ctes = @ctes
      old_windows = @window_values
      @ctes = @ctes.dup
      begin
        materialize_ctes(stmt)
        return select(stmt[:query]) if stmt[:kind] == :with
        return select_values(stmt) if stmt[:kind] == :values
        return select_set(stmt) if %i[set_operation setop].include?(stmt[:kind])
        raise Error.new('0A000', 'DISTINCT ON is not supported') if stmt[:distinct_on]
        from = stmt[:from] || []
        from = [from] unless from.is_a?(Array)
        raise Error.new('42601', 'SELECT * requires a FROM relation') if from.empty? && (stmt[:columns] || []).any? { |item| item.dig(:expr, :kind) == :star }
        rel = Relation.new(columns: [], rows: [[]])
        predicate = nil
        where_node = stmt[:where]
        if where_node
          analyzer = ParameterBindings.new(self, nil)
          outer_scope = @outer.flat_map(&:first)
          metadata = query_input_columns(stmt, @ctes.transform_values(&:columns), outer_scope, analyzer)
          metadata = metadata.take(metadata.length - outer_scope.length)
          resolved_where = compile_filter(where_node, metadata)
          contradiction = where_contradiction?(resolved_where)
        end
        if contradiction
          rel = Relation.new(columns: metadata, rows: [])
          predicate = resolved_where
        elsif from.length == 1 && from[0][:kind] == :table && !from[0][:columns] && !from[0][:column_aliases] && @tables.key?(from[0][:name]) && !@ctes.key?(from[0][:name]) && (stmt[:joins] || []).empty?
          source = from[0]
          stored = table(source[:name])
          scope = table_columns(stored, source[:alias] || source[:name])
          predicate = compile_filter(stmt[:where], scope)
          scan = primary_scan(stored, predicate)
          if scan
            charge(scan.length)
            rel = Relation.new(columns: scope, rows: scan)
            query_memory_relation(rel, mask: query_memory_mask(scope)) if @query_memory
          else
            rel = source_relation(source)
          end
        else
          rel, where_node = materialize_from(from, stmt[:joins] || [], where_node)
        end
        predicate ||= compile_filter(where_node, rel.columns)
        profile_checkpoint(:scan) if @explain_profile_frames
        filter_kernel = native_compiled_relation_filter(predicate, memory: !!@query_memory) if rel.rows.is_a?(Array)
        rel = if filter_kernel
          if @query_memory
            owner = rel.instance_variable_get(:@native_memory_owner)
            mask = rel.instance_variable_get(:@native_memory_mask)
            rows = filter_kernel.call(self, rel.rows, @params, owner, rel.columns, mask)
            query_memory_transfer(rel, Relation.new(columns: rel.columns, rows: rows))
          else
            Relation.new(columns: rel.columns, rows: filter_kernel.call(self, rel.rows, @params))
          end
        elsif @query_memory
          query_memory_filter(rel) { |row| !predicate || evaluate(predicate, row) == true }
        else
          Relation.new(columns: rel.columns, rows: rel.rows.select { |row| !predicate || evaluate(predicate, row) == true })
        end
        profile_checkpoint(:filter) if @explain_profile_frames
        projections = expand_stars(stmt[:columns] || stmt[:projections], rel.columns)
        compiled = projections.map { |p| compile(p[:expr], rel.columns) }
        group_nodes = respond_to?(:resolve_group_nodes) ? resolve_group_nodes(stmt, rel.columns) : (stmt[:group_by] || [])
        grouped = !group_nodes.empty? || !!stmt[:having] || compiled.any? { |e| aggregate_expr?(e) } || contains_aggregate?(stmt[:windows])
        complex_groups = group_nodes.any? { |g| %i[grouping_sets rollup cube].include?(g[:kind]) }
        group_exprs = (complex_groups ? [] : group_nodes).map { |g| compile(g, rel.columns) }
        group_exprs.each { |expression| ensure_comparable_type(expression.type) }
        compiled.each { |expression| ensure_comparable_type(expression.type) } if stmt[:distinct]
        validate_aggregate_scope(stmt, rel.columns, grouped) if respond_to?(:validate_aggregate_scope)
        contexts = if grouped && respond_to?(:build_aggregate_contexts)
          build_aggregate_contexts(stmt, rel, group_nodes)
        elsif grouped
          groups = {}
          rel.rows.each do |row|
            key = group_exprs.map { |e| evaluate(e, row) }
            (groups[key] ||= []) << row
          end
          groups[[]] ||= [] if group_exprs.empty?
          having = compile_filter(stmt[:having], rel.columns)
          groups.values.filter_map do |group|
            row = group.first || Array.new(rel.columns.length)
            next if having && evaluate(having, row, group) != true
            [row, group]
          end
        else
          rel.rows.map { |row| [row, nil] }
        end
        profile_checkpoint(:aggregate) if @explain_profile_frames
        columns = projections.zip(compiled).map { |p, e| { name: p[:alias] || expression_name(p[:expr]), type: e.type } }
        prepare_window_values(stmt, rel.columns, contexts) if respond_to?(:prepare_window_values)
        profile_checkpoint(:window) if @explain_profile_frames
        defer_projection = !stmt[:distinct] && (cursor_output? || !grouped && !contains_window?(stmt[:columns]))
        if defer_projection
          output_owner = query_memory_owner if @query_memory && query_memory_collect_result?
          results = contexts.map { |row, group| [nil, row, group, {}] }
        elsif @query_memory
          output_owner = query_memory_owner if stmt[:distinct] || query_memory_collect_result?
          distinct_owner = query_memory_owner if stmt[:distinct]
          seen = {} if stmt[:distinct]
          results = contexts.filter_map do |row, group|
            values = compiled.map { |e| evaluate(e, row, group) }
            if seen
              next if seen.key?(values)
              query_memory_add(distinct_owner, MemorySchedule::ENTRY + query_memory_key_bytes(values, compiled.map(&:type)))
              seen[values] = true
            end
            query_memory_add_row(output_owner, values, columns) if output_owner
            [values, row, group]
          end
          query_memory_release(distinct_owner) if distinct_owner
          query_memory_release(rel) if stmt[:distinct]
        else
          results = contexts.map do |row, group|
            [compiled.map { |e| evaluate(e, row, group) }, row, group]
          end
          results = results.uniq { |entry| entry[0] } if stmt[:distinct]
        end
        profile_checkpoint(:distinct) if @explain_profile_frames
        orders = stmt[:order_by] || []
        unless orders.empty?
          order_exprs = orders.map do |order|
            node = order[:expr] || order
            if node[:kind] == :literal && node[:value].is_a?(Integer)
              idx = node[:value] - 1
              raise Error.new('42P10', 'ORDER BY position out of range') unless idx.between?(0, columns.length - 1)
              [:output, idx, order]
            elsif node[:kind] == :column && !node[:table] && (idx = columns.index { |c| c[:name] == node[:name] })
              [:output, idx, order]
            else
              [:input, compile(node, rel.columns), order]
            end
          end
          order_exprs.each { |kind, expression, _| ensure_comparable_type(kind == :output ? columns[expression][:type] : expression.type) }
          sort_memory_owner = query_memory_owner if @query_memory
          keyed = results.map do |entry|
            keys = order_exprs.map do |kind, expression, _|
              if kind == :output && defer_projection
                entry[3].fetch(expression) { entry[3][expression] = evaluate(compiled[expression], entry[1], entry[2]) }
              elsif kind == :output
                entry[0][expression]
              else
                evaluate(expression, entry[1], entry[2])
              end
            end
            query_memory_add(sort_memory_owner, query_memory_collation_bytes(keys, sort_memory_owner)) if @query_memory
            [entry, keys]
          end
          keyed.sort! do |a, b|
            comparison = 0
            order_exprs.each_with_index do |(_, _, order), i|
              comparison = order_compare(a[1][i], b[1][i], order)
              break unless comparison.zero?
            end
            comparison
          end
          results = keyed.map(&:first)
          query_memory_release(sort_memory_owner) if @query_memory
        end
        profile_checkpoint(:sort) if @explain_profile_frames
        offset = stmt[:offset] ? evaluate(compile(stmt[:offset], []), []) : 0
        limit = stmt[:limit] ? evaluate(compile(stmt[:limit], []), []) : nil
        offset ||= 0
        raise Error.new('2201X', 'negative OFFSET') if offset.negative?
        raise Error.new('2201W', 'negative LIMIT') if limit && limit.negative?
        results = results.drop(offset)
        results = results.take(limit) if limit
        cursor_rows = cursor_output(columns) if cursor_output?
        if defer_projection
          host_batch_rows(results, compiled, row_at: ->(entry) { entry[1] }).each do |entry|
            charge(0)
            @cost += 1
            @lifetime_cost += 1
            @cursor_session.engine.instance_variable_set(:@lifetime_cost, @lifetime_cost) if @cursor_session
            entry[0] = compiled.each_with_index.map do |expression, index|
              charge(0)
              evaluate(expression, entry[1], entry[2])
            end
            query_memory_add_row(output_owner, entry[0], columns) if output_owner
            if cursor_rows
              cursor_rows << entry[0]
              entry[0] = nil
            end
          end
        end
        if cursor_rows && !defer_projection
          results.each { |entry| charge; cursor_rows << entry[0] }
        elsif !cursor_rows && !defer_projection
          # The output checkpoint precedes each row's accrual. There is no
          # trailing guard after the final produced row (cost.md §6).
          if @max_cost.positive? || @lifetime_max_cost.positive?
            results.length.times do
              charge(0)
              @cost += 1
              @lifetime_cost += 1
              @cursor_session.engine.instance_variable_set(:@lifetime_cost, @lifetime_cost) if @cursor_session
            end
          else
            @cost += results.length
            @lifetime_cost += results.length
            @cursor_session.engine.instance_variable_set(:@lifetime_cost, @lifetime_cost) if @cursor_session
          end
        end
        profile_checkpoint(:root) if @explain_profile_frames
        result = Relation.new(columns: columns, rows: cursor_rows || results.map(&:first))
        query_memory_attach(result, output_owner) if output_owner
        result
      ensure
        @ctes = old_ctes
        @window_values = old_windows
      end
    end

    def source_relation(source)
      case source[:kind]
      when :table, nil
        name = source[:name]
        if @ctes.key?(name)
          rel = @ctes[name]
          charge(rel.rows.length)
        elsif @views.key?(name)
          rel = select(source[:native_body_query] || @views[name][:query])
        else
          t = table(name)
          bound_columns = table_columns(t, source[:alias] || name)
          bound_columns = bound_columns.each_with_index.map { |column, i| column.merge(name: source[:columns]&.[](i) || column[:name]) } if source[:columns]
          bound = source[:native_access_predicate] && compile_filter(source[:native_access_predicate], bound_columns)
          input = if source[:native_access_plan]
            execute_scan_access(t, bound, source[:native_access_plan])
          else
            bound && primary_scan(t, bound)
          end
          unless input
            charge(full_scan_pages(t))
            charge_scan_payload(t)
            input = t[:native_primary_entries] ? t[:native_primary_entries].map { |_, index| t[:rows][index] } : t[:rows]
          end
          charge(input.length)
          rel = Relation.new(columns: table_columns(t), rows: input)
        end
      when :subquery, :derived
        rel = select(source[:query])
      when :join_group
        rel, = materialize_from(source[:from], source[:joins])
        charge(rel.rows.length) unless source[:native_inline_group]
      when :function, :call, :srf
        call = source[:expr] || source[:call] || source
        args = (call[:args] || []).map { |a| evaluate(compile(a, []), []) }
        name = call[:name]
        case name
        when 'generate_series'
          first, last, step = args
          step = 1 if args.length < 3
          raise Error.new('22023', 'zero series step') if step && step.zero?
          rows = []
          memory_owner = query_memory_owner if @query_memory
          unless first.nil? || last.nil? || step.nil?
            while step.positive? ? first <= last : first >= last
              charge
              query_memory_add_row(memory_owner, [first]) if @query_memory
              rows << [first]
              first += step
            end
          end
          rel = Relation.new(columns: [{ name: name, type: 'i64' }], rows: rows)
          query_memory_attach(rel, memory_owner) if @query_memory
        when 'unnest'
          values = args.first
          items = values.respond_to?(:elements) ? values.elements : Array(values)
          memory_owner = query_memory_owner if @query_memory
          rows = items.map do |value|
            charge
            row = [value]
            query_memory_add_row(memory_owner, row) if @query_memory
            row
          end
          rel = Relation.new(columns: [{ name: name, type: Types.infer(items.find { |v| !v.nil? }) }], rows: rows)
          query_memory_attach(rel, memory_owner) if @query_memory
        else raise Error.new('0A000', "table function #{name} is not implemented")
        end
      else raise Error.new('0A000', "FROM #{source[:kind]} is not implemented")
      end
      alias_name = source[:alias] || source[:name]
      column_names = source[:columns] || source[:column_aliases]
      if source[:kind] == :join_group && alias_name
        visible = rel.columns.each_index.reject { |i| rel.columns[i][:qualified_only] }
        rel = Relation.new(columns: visible.map { |i| rel.columns[i] }, rows: rel.rows.map { |row| visible.map { |i| row[i] } })
      end
      columns = rel.columns.each_with_index.map do |c, i|
        name = column_names&.dig(i) || (source[:kind] == :function && rel.columns.length == 1 ? source[:alias] : nil) || c[:name]
        ordinal = source[:kind] == :join_group && !alias_name ? c[:source_ordinal] : i
        c.merge(table: alias_name || c[:table], name: name, source_ordinal: ordinal)
      end
      result = Relation.new(columns: columns, rows: rel.rows)
      query_memory_transfer(rel, result) if @query_memory
      result
    end

    def join(left, source, kind, on, using)
      profile_start = @cost if source[:native_join_profile]
      dependency = lateral_source_dependency?(source, left.columns)
      right_preserved = %i[right full right_outer full_outer].include?(kind)
      lateral = dependency || source[:lateral] && !right_preserved
      if dependency && right_preserved
        raise Error.new('42P10', 'invalid lateral reference in RIGHT/FULL JOIN')
      end
      index_plan = source[:native_index_plan]
      index_plan ||= join_index_plan(left, source, kind, on, using) unless lateral || source[:native_join_algorithm]
      right = if source[:native_materialized]
        original = source[:native_materialized]
        Relation.new(columns: original.columns, rows: original.rows)
      elsif index_plan
        Relation.new(columns: index_plan[:columns], rows: [])
      elsif lateral
        if left.rows.empty?
          metadata = ParameterBindings.new(self, nil).relation_columns(source, left.columns)
          Relation.new(columns: metadata, rows: [])
        else
          lateral_source_relation(source, left.columns, left.rows.first)
        end
      else
        source_relation(source)
      end
      columns = left.columns + right.columns
      left_names = left.columns.filter_map { |c| c[:table] }.reject { |name| name.start_with?('*') }.uniq
      right_names = right.columns.filter_map { |c| c[:table] }.reject { |name| name.start_with?('*') }.uniq
      raise Error.new('42712', 'table name specified more than once') unless (left_names & right_names).empty?
      outer = @outer
      begin
        @outer = outer.reject { |scope| (@join_group_outer || []).any? { |hidden| hidden.equal?(scope) } }
        predicate = compile_filter(on, columns)
        on_trees = source[:native_on_trees]&.map { |node| compile_filter(node, columns) }
      ensure
        @outer = outer
      end
      raise Error.new('0A000', 'FULL JOIN USING/NATURAL is not supported') if using && right_preserved && %i[full full_outer].include?(kind)
      using = left.columns.reject { |c| c[:qualified_only] }.map { |c| c[:name] } & right.columns.reject { |c| c[:qualified_only] }.map { |c| c[:name] } if using == :natural
      pairs = (using || []).map do |name|
        ls = left.columns.each_index.select { |i| left.columns[i][:name] == name && !left.columns[i][:qualified_only] }
        rs = right.columns.each_index.select { |i| right.columns[i][:name] == name && !right.columns[i][:qualified_only] }
        raise Error.new('42702', "ambiguous USING column #{name}") if ls.length > 1 || rs.length > 1
        l, r = ls.first, rs.first
        raise Error.new('42703', "unknown USING column #{name}") unless l && r
        [l, r]
      end
      unless pairs.empty?
        using_predicates = pairs.map do |li, ri|
          { kind: :binary, op: '=', left: { kind: :column, native_ordinal: li },
            right: { kind: :column, native_ordinal: left.columns.length + ri } }
        end
        predicate = compile_filter(join_conjunctions(using_predicates), columns)
      end
      rows, right_seen = [], {}
      memory_owner = query_memory_owner if @query_memory
      memory_mask = query_memory_mask(columns) if @query_memory
      hash_gate = source[:native_original_on] ? compile_filter(source[:native_original_on], columns) : predicate
      hash_table = hash_join_table(left, right, predicate, kind, gate: hash_gate, force: source[:native_join_algorithm] == :hash || source[:native_fixed_hash]) unless lateral || index_plan || source[:native_wide_outer] || source[:native_join_algorithm] == :nested
      window = source[:native_join_window]
      compile_predicates = !@native_compilation_disabled && !@native_join_compilation_disabled
      if compile_predicates && !@native_compilation_immediate && !lateral && left.rows.instance_of?(Array)
        # This is an input-size heuristic: indexed joins use the outer count,
        # and other joins use the product. Never inspect lazy source lengths or
        # perform a charged hash/index probe to decide compilation admission.
        if index_plan
          input_size = left.rows.length
        elsif right.rows.instance_of?(Array)
          input_size = left.rows.length * right.rows.length
        end
        compile_predicates = input_size >= CompiledPlans::MIN_JOIN_PREDICATE_CANDIDATES if input_size
      end
      if compile_predicates
        on_programs = nil
        on_trees&.each_with_index do |expression, index|
          kernel = native_compiled_join_predicate(expression, input_size: input_size)
          next unless kernel
          on_programs ||= on_trees.map { |tree| [tree, nil] }
          on_programs[index][1] = kernel
        end
        predicate_kernel = native_compiled_join_predicate(predicate, input_size: input_size) unless on_trees
        window_kernel = native_compiled_join_predicate(window[:filter], input_size: input_size) if window && window[:filter]
      end
      if index_plan && !(left.rows.instance_of?(Array) && left.rows.length <= 1)
        index_plan = prepare_join_index_execution(index_plan)
      end
      left.rows.each_with_index do |lrow, left_index|
        break if window && rows.length >= window[:target]
        right = join_index_relation(index_plan, left, lrow) if index_plan
        right = lateral_source_relation(source, left.columns, lrow) if lateral && left_index.positive?
        matched = false
        candidates = hash_table ? hash_join_candidates(hash_table, lrow) : right.rows.each_index
        candidates.each do |index|
          rrow = right.rows[index]
          row = lrow + rrow.to_a
          if on_trees
            accepted = if on_programs
              on_programs.all? { |expression, kernel| (kernel ? kernel.call(self, row, @params) : evaluate(expression, row)) == true }
            else
              on_trees.all? { |expression| evaluate(expression, row) == true }
            end
            next unless accepted
          elsif predicate
            next if (predicate_kernel ? predicate_kernel.call(self, row, @params) : evaluate(predicate, row)) != true
          end
          next if window && window[:filter] &&
            (window_kernel ? window_kernel.call(self, row, @params) : evaluate(window[:filter], row)) != true
          query_memory_add_row(memory_owner, row, columns, memory_mask) if @query_memory
          query_memory_add(memory_owner, (source[:native_full_join_width] - columns.length) * MemorySchedule::VALUE) if @query_memory && source[:native_full_join_width]
          rows << row
          matched = true
          right_seen[index] = true
          break if window && rows.length >= window[:target]
        end
        if !matched && %i[left full left_outer full_outer].include?(kind)
          row = lrow + Array.new(right.columns.length)
          query_memory_add_row(memory_owner, row, columns, memory_mask) if @query_memory
          query_memory_add(memory_owner, (source[:native_full_join_width] - columns.length) * MemorySchedule::VALUE) if @query_memory && source[:native_full_join_width]
          rows << row
        end
        query_memory_release(right) if @query_memory && (lateral || index_plan)
      end
      if %i[right full right_outer full_outer].include?(kind)
        right.rows.each_with_index do |row, index|
          next if right_seen[index]
          combined = Array.new(left.columns.length) + row.to_a
          query_memory_add_row(memory_owner, combined, columns, memory_mask) if @query_memory
          rows << combined
        end
      end
      unless pairs.empty?
        merged = pairs.map { |li, ri| left.columns[li].merge(table: nil, type: common_type(left.columns[li][:type], right.columns[ri][:type])) }
        left_ordinals, right_ordinals = pairs.transpose
        visible = columns.each_index.reject { |i| left_ordinals.include?(i) || right_ordinals.include?(i - left.columns.length) }
        hidden = pairs.flat_map { |li, ri| [li, left.columns.length + ri] }
        rows = rows.map do |row|
          pairs.each_with_index.map do |(li, ri), index|
            value = row[li].nil? ? row[left.columns.length + ri] : row[li]
            cast_value(value, merged[index][:type])
          end + visible.map { |i| row[i] } + hidden.map { |i| row[i] }
        end
        columns = merged + visible.map { |i| columns[i] } + hidden.map { |i| columns[i].merge(qualified_only: true) }
      end
      result = Relation.new(columns: columns, rows: rows)
      query_memory_attach(result, memory_owner, memory_mask) if @query_memory
      source[:native_join_profile].actual = (source[:native_join_left_node].actual || 0) + @cost - profile_start if profile_start
      result
    ensure
      if @query_memory
        query_memory_release(left)
        query_memory_release(right) if right
        hash_join_release(hash_table) if hash_table && respond_to?(:hash_join_release)
      end
    end

    def lateral_source_dependency?(source, columns)
      if source[:kind] == :join_group
        (source[:from] + source[:joins].map { |item| item[:relation] }).any? { |item| lateral_source_dependency?(item, columns) }
      elsif source[:lateral] || %i[function call srf json_table].include?(source[:kind])
        source_uses_columns?(source[:query] || source[:expr] || source.reject { |key, _| key.to_s.start_with?('native_') }, columns)
      else
        false
      end
    end

    def lateral_source_relation(source, columns, row)
      if source[:kind] != :join_group && @join_group_outer&.any?
        prefix_columns = @join_group_outer.flat_map(&:first)
        prefix_values = @join_group_outer.flat_map { |_, values| values.to_a }
        columns = prefix_columns + columns
        row = prefix_values + row.to_a
      end
      correlated(columns, row) do
        if source[:kind] == :join_group
          @join_group_outer ||= []
          @join_group_outer << @outer.last
          begin
            source_relation(source)
          ensure
            @join_group_outer.pop
          end
        else
          source_relation(source)
        end
      end
    end

    def materialize_from(sources, joins, predicate = nil)
      relation = Relation.new(columns: [], rows: [[]])
      query_memory_relation(relation) if @query_memory && sources.empty?
      pushable = joins.all? { |item| %i[inner cross].include?(item[:kind]) }
      sources.each_with_index do |source, index|
        group_joins = joins.select { |item| (item[:from_index] || 0) == index }
        # A comma separates joined-table expressions. Its preceding relations
        # form an outer scope, so a RIGHT JOIN can reference an earlier comma
        # sibling without depending on its own nullable left input.
        if !group_joins.empty? && sources.length > 1
          component = @explain_runtime_plans && @explain_group_components&.[](source.object_id)
          component ||= { kind: :join_group, from: [source], joins: group_joins.map { |item| item.merge(from_index: 0) },
                        native_inline_group: index.zero? || group_joins.all? { |item| %i[inner cross].include?(item[:kind]) } }
          relation = index.zero? ? source_relation(component) : join(relation, component, :cross, nil, nil)
        else
          relation = index.zero? ? source_relation(source) : join(relation, source, :cross, nil, nil)
          group_joins.each do |item|
            relation = join(relation, item[:relation] || item[:right], item[:kind] || item[:type], item[:on], item[:natural] ? :natural : item[:using])
          end
        end
        if pushable && index < sources.length - 1 && predicate
          terms = ast_conjunctions(predicate)
          local, remaining = terms.partition { |term| local_filter?(term, relation.columns) }
          unless local.empty?
            expression = compile_filter(join_conjunctions(local), relation.columns)
            relation = if @query_memory
              query_memory_filter(relation) { |row| evaluate(expression, row) == true }
            else
              Relation.new(columns: relation.columns, rows: relation.rows.select { |row| evaluate(expression, row) == true })
            end
            predicate = join_conjunctions(remaining)
          end
        end
      end
      [relation, predicate]
    end

    def ast_conjunctions(node)
      node[:kind] == :binary && node[:op] == 'and' ? ast_conjunctions(node[:left]) + ast_conjunctions(node[:right]) : [node]
    end

    def join_conjunctions(nodes)
      nodes.reduce { |left, right| { kind: :binary, op: 'and', left: left, right: right } }
    end

    def local_filter?(node, columns)
      case node
      when Array then node.all? { |child| local_filter?(child, columns) }
      when Hash
        return false if %i[subquery exists array_query].include?(node[:kind])
        if node[:kind] == :column
          return columns.count { |column| column[:name] == node[:name] && (node[:table] ? column[:table] == node[:table] : !column[:qualified_only]) } == 1
        end
        if node[:kind] == :call
          return false if Functions.aggregate?(node[:name]) || FunctionCatalog::VOLATILITY.fetch(node[:name], []).any? { |_, volatility| volatility != 'immutable' }
        end
        node.each_value.all? { |child| local_filter?(child, columns) }
      else true
      end
    end

    def source_uses_columns?(node, columns)
      case node
      when Array then node.any? { |item| source_uses_columns?(item, columns) }
      when Hash
        if node[:kind] == :column
          unless node[:table]
            prefix = (@join_group_outer || []).flat_map(&:first)
            matches = (prefix + columns).select { |column| column[:name] == node[:name] && !column[:qualified_only] }
            raise Error.new('42702', "ambiguous column #{node[:name]}") if matches.length > 1
          end
          columns.any? { |column| column[:name] == node[:name] && (!node[:table] || column[:table] == node[:table]) }
        else
          node.values.any? { |value| source_uses_columns?(value, columns) }
        end
      else false
      end
    end

    def select_values(stmt)
      compiled = stmt[:rows].map { |row| row.map { |e| compile(e, []) } }
      width = compiled.first&.length || 0
      raise Error.new('42601', 'VALUES width mismatch') unless compiled.all? { |row| row.length == width }
      columns = width.times.map do |i|
        type = compiled.map { |row| row[i].type }.reduce('unknown') { |a, b| common_type(a, b) }
        { name: "column#{i + 1}", type: type }
      end
      rows = compiled.map { |row| row.each_with_index.map { |e, i| cast_value(evaluate(e, []), columns[i][:type]) } }
      charge(rows.length)
      Relation.new(columns: columns, rows: rows)
    end

    def select_set(stmt)
      left, right = select(stmt[:left]), select(stmt[:right])
      raise Error.new('42601', 'set-operation width mismatch') unless left.columns.length == right.columns.length
      columns = left.columns.each_with_index.map { |c, i| c.merge(type: common_type(c[:type], right.columns[i][:type])) }
      lrows, rrows = [left, right].map { |rel| rel.rows.map { |row| row.each_with_index.map { |v, i| cast_value(v, columns[i][:type]) } } }
      operation = (stmt[:op] || stmt[:operator]).to_s.downcase
      rows = case operation
      when 'union' then stmt[:all] ? lrows + rrows : (lrows + rrows).uniq
      when 'intersect', 'except'
        counts = rrows.tally
        input = stmt[:all] ? lrows : lrows.uniq
        input.select do |row|
          found = counts.fetch(row, 0).positive?
          counts[row] -= 1 if found && stmt[:all]
          operation == 'intersect' ? found : !found
        end
      else raise Error.new('0A000', "set operation #{operation}")
      end
      Relation.new(columns: columns, rows: rows)
    end

    def expand_stars(projections, columns)
      if columns.empty? && projections.any? { |projection| projection.dig(:expr, :kind) == :star }
        raise Error.new('42601', 'SELECT * requires a FROM relation')
      end
      projections.flat_map do |projection|
        expr = projection[:expr]
        if expr[:kind] == :star
          selected = columns.each_index.select { |i| !columns[i][:star_hidden] && (expr[:table] ? columns[i][:table] == expr[:table] : !columns[i][:qualified_only]) }
          selected = selected.sort_by { |i| columns[i][:source_ordinal] || 0 } if expr[:table]
          raise Error.new('42P01', 'unknown relation in qualified star') if expr[:table] && selected.empty?
          selected.map do |i|
            c = columns[i]
            { expr: { kind: :column, table: c[:table], name: c[:name], native_ordinal: i }, alias: c[:name] }
          end
        else
          [projection]
        end
      end
    end

    def project(rel, projections)
      projections = expand_stars(projections, rel.columns)
      compiled = projections.map { |p| compile(p[:expr], rel.columns) }
      columns = projections.zip(compiled).map { |p, e| { name: p[:alias] || expression_name(p[:expr]), type: e.type } }
      rows = rel.rows.map { |row| compiled.map { |e| evaluate(e, row) } }
      charge(rows.length)
      Relation.new(columns: columns, rows: rows)
    end

    def expression_name(node)
      case node[:kind]
      when :column then node[:name]
      when :call then node[:name]
      when :cast then expression_name(node[:expr])
      else '?column?'
      end
    end

    def order_compare(a, b, order)
      direction = order[:direction]
      desc = order[:desc] || direction == :desc || direction == 'desc'
      unless desc || direction == :asc || direction == 'asc' || direction.nil?
        desc = direction.to_s.downcase == 'desc'
      end
      return 0 if a.nil? && b.nil?
      if a.nil? || b.nil?
        nulls = order[:nulls] || (desc ? :first : :last)
        first = nulls == :first || nulls == 'first'
        first ||= nulls.to_s == 'first' unless nulls == :last || nulls == 'last'
        return first ? -1 : 1 if a.nil?
        return first ? 1 : -1
      end
      n = Types.compare(a, b)
      desc ? -n : n
    end

    def common_type(a, b)
      return b if a == 'unknown'
      return a if b == 'unknown' || a == b
      Types.promote(a, b)
    end

    def cast_value(value, type, source_type: nil)
      return nil if value.nil?
      if @types.key?(type)
        fields = @types[type]
        text_input = value.is_a?(String)
        values = text_input ? Types.split_container(value[1...-1]) : value
        raise Error.new('42846', "cannot cast to #{type}") unless values.is_a?(Array) && values.length == fields.length
        values = fields.each_with_index.map do |field, i|
          input = text_input ? cast_value(values[i], field[:type]) : values[i]
          assign(input, field)
        end
        CompositeValue.new(values, type_name: type, field_names: fields.map { |f| f[:name] })
      elsif type.end_with?('[]') && @types.key?(type[0...-2])
        ArrayValue.new(value.map { |v| cast_value(v, type[0...-2]) }, element_type: type[0...-2])
      else
        Types.cast(value, type, source_type: source_type)
      end
    end

    def compile_filter(node, columns)
      return nil unless node
      expression = compile(node, columns)
      raise Error.new('42804', 'predicate must be boolean') unless %w[boolean unknown null].include?(expression.type)
      expression
    end

    def compile(node, columns)
      raise Error.new('42601', 'missing expression') unless node
      kind = node[:kind]
      case kind
      when :literal
        type = node[:type] || Types.infer(node[:value])
        value = type == 'decimal' ? cast_value(node[:value], type) : node[:value]
        Expression.new(kind: kind, type: type, value: value, extra: node)
      when :param
        i = node[:index] - 1
        raise Error.new('42601', 'bind parameter count mismatch') if i.negative? || i >= @params.length
        Expression.new(kind: kind, type: Types.infer(@params[i]), value: i)
      when :column
        if node.key?(:native_ordinal)
          index = node[:native_ordinal]
          return Expression.new(kind: :column, type: columns.fetch(index)[:type], value: index)
        end
        matches = columns.each_index.select { |i| columns[i][:name] == node[:name] && (node[:table] ? columns[i][:table] == node[:table] : !columns[i][:qualified_only]) }
        raise Error.new(node[:table] == 'excluded' ? '42P09' : '42702', "ambiguous column #{node[:name]}") if matches.length > 1
        if matches.empty?
          @outer.reverse_each.with_index do |outer, depth|
            positions = outer[0].each_index.select { |index| outer[0][index][:name] == node[:name] && (node[:table] ? outer[0][index][:table] == node[:table] : !outer[0][index][:qualified_only]) }
            raise Error.new('42702', "ambiguous column #{node[:name]}") if positions.length > 1
            i = positions.first
            return Expression.new(kind: :outer, type: outer[0][i][:type], value: [depth, i]) if i
          end
          code = node[:table] && !columns.any? { |c| c[:table] == node[:table] } ? '42P01' : '42703'
          raise Error.new(code, "unknown column #{node[:name]}")
        end
        i = matches.first
        Expression.new(kind: :column, type: columns[i][:type], value: i)
      when :cast
        Expression.new(kind: kind, type: Types.normalize(node[:type]), args: [compile(node[:expr], columns)], extra: node)
      when :unary
        arg = compile(node[:expr] || node[:operand], columns)
        op = node[:op].to_s.downcase
        type = op.start_with?('is ') || op == 'not' ? 'boolean' : arg.type
        Expression.new(kind: kind, type: type, value: op, args: [arg])
      when :binary
        left, right = compile(node[:left], columns), compile(node[:right], columns)
        op = node[:op].to_s.downcase
        contextual_binary(op, left, right) if respond_to?(:contextual_binary)
        comparison = %w[= <> != < <= > >= and or like ilike not\ like not\ ilike ~ ~* !~ !~* is\ distinct\ from is\ not\ distinct\ from @> <@ && ? ?| ?&].include?(op)
        type = respond_to?(:binary_type) ? binary_type(op, left.type, right.type) : (comparison ? 'boolean' : common_type(left.type, right.type))
        if %w[and or].include?(op) && [left.type, right.type].any? { |t| !%w[boolean unknown].include?(t) }
          raise Error.new('42804', 'boolean operands required')
        end
        Expression.new(kind: kind, type: type, value: op, args: [left, right])
      when :call
        name = node[:name].downcase
        args = (node[:args] || []).map { |a| a[:kind] == :star ? Expression.new(kind: :star, type: 'unknown') : compile(a, columns) }
        type = case name
        when 'count' then 'i64'
        when 'sum' then %w[i16 i32].include?(args[0]&.type) ? 'i64' : (args[0]&.type == 'i64' ? 'decimal' : args[0]&.type)
        when 'avg' then %w[f32 f64].include?(args[0]&.type) ? 'f64' : 'decimal'
        when 'min', 'max' then args[0]&.type
        when 'nextval', 'currval', 'setval', 'lastval' then 'i64'
        when 'coalesce', 'greatest', 'least', 'nullif' then args.map(&:type).reduce('unknown') { |a, b| common_type(a, b) }
        else Functions.return_type(name, args.map(&:type))
        end
        extra = node.merge(filter_expr: compile_boolean_value(node[:filter], columns))
        Expression.new(kind: kind, type: type || 'unknown', value: name, args: args, extra: extra)
      when :case
        operand = node[:operand] ? compile(node[:operand], columns) : nil
        branches = node[:branches].map do |branch|
          condition, value = branch.is_a?(Array) ? branch : [branch[:when], branch[:then]]
          [compile(condition, columns), compile(value, columns)]
        end
        fallback = node[:else] ? compile(node[:else], columns) : Expression.new(kind: :literal, type: 'unknown', value: nil)
        type = (branches.map { |_, e| e.type } + [fallback.type]).reduce('unknown') { |a, b| common_type(a, b) }
        Expression.new(kind: kind, type: type, args: [operand, branches, fallback])
      when :in
        args = [compile(node[:expr] || node[:left], columns)]
        args.concat((node[:values] || node[:list] || []).map { |e| compile(e, columns) })
        Expression.new(kind: kind, type: 'boolean', args: args, extra: node.merge(scope: columns))
      when :between
        Expression.new(kind: kind, type: 'boolean', args: [compile(node[:expr], columns), compile(node[:low] || node[:lower], columns), compile(node[:high] || node[:upper], columns)], extra: node)
      when :array, :row
        args = (node[:elements] || node[:args] || node[:values]).map { |e| compile(e, columns) }
        type = kind == :row ? 'record' : "#{args.map(&:type).reduce('unknown') { |a, b| common_type(a, b) }}[]"
        Expression.new(kind: kind, type: type, args: args)
      when :subquery, :exists
        Expression.new(kind: kind, type: kind == :exists ? 'boolean' : 'unknown', value: node[:query], extra: { scope: columns, negated: node[:negated] })
      else raise Error.new('0A000', "expression #{kind} is not implemented")
      end
    end

    def aggregate_expr?(e)
      return true if e.kind == :call && Functions.aggregate?(e.value) && !e.extra[:over]
      (e.args || []).any? { |a| a.is_a?(Expression) && aggregate_expr?(a) }
    end

    def contains_aggregate?(node)
      return false unless node.is_a?(Hash)
      return true if node[:kind] == :call && Functions.aggregate?(node[:name]) && !node[:over]
      node.values.any? { |v| v.is_a?(Array) ? v.any? { |e| contains_aggregate?(e) } : contains_aggregate?(v) }
    end

    def evaluate(expr, row, group = nil)
      case expr.kind
      when :literal then expr.value
      when :column then row[expr.value]
      when :outer then @outer[-1 - expr.value[0]][1][expr.value[1]]
      when :param then expr.extra&.dig(:context_type) ? cast_value(@params[expr.value], expr.type) : @params[expr.value]
      when :cast
        charge
        cast_value(evaluate(expr.args[0], row, group), expr.type, source_type: expr.args[0].type)
      when :unary
        charge
        value = evaluate(expr.args[0], row, group)
        case expr.value
        when 'is null' then value.nil?
        when 'is not null' then !value.nil?
        when 'is true' then value == true
        when 'is false' then value == false
        when 'is not true' then value != true
        when 'is not false' then value != false
        when 'is unknown' then value.nil?
        when 'is not unknown' then !value.nil?
        else Types.unary(expr.value, value, type: expr.type)
        end
      when :binary
        charge
        a = evaluate(expr.args[0], row, group)
        return false if expr.value == 'and' && a == false
        return true if expr.value == 'or' && a == true
        b = evaluate(expr.args[1], row, group)
        return integer_binary(expr, a, b) if expr.extra&.dig(:integer_kernel)
        case expr.value
        when 'and' then a == false || b == false ? false : (a.nil? || b.nil? ? nil : true)
        when 'or' then a == true || b == true ? true : (a.nil? || b.nil? ? nil : false)
        when 'is distinct from' then a.nil? || b.nil? ? a != b : Types.compare(a, b) != 0
        when 'is not distinct from' then a.nil? || b.nil? ? a == b : Types.compare(a, b) == 0
        else Types.binary(expr.value, a, b, type: expr.type)
        end
      when :call
        return aggregate(expr, group) if Functions.aggregate?(expr.value) && group
        raise Error.new('0A000', 'window execution is not implemented') if expr.extra[:over]
        charge
        if expr.value == 'coalesce'
          expr.args.each do |arg|
            value = evaluate(arg, row, group)
            return cast_value(value, expr.type) unless value.nil?
          end
          return nil
        end
        args = expr.args.map { |arg| evaluate(arg, row, group) }
        case expr.value
        when 'nextval', 'currval' then sequence_value(args[0], expr.value)
        when 'setval' then sequence_value(args[0], expr.value, args[1], args.length < 3 || args[2])
        when 'lastval'
          raise Error.new('55000', 'no previous nextval') if @sequence_last.nil?
          @sequence_session.fetch(@sequence_last)
        else Functions.call(expr.value, args, context: self, arg_types: expr.args.map(&:type))
        end
      when :case
        charge
        operand, branches, fallback = expr.args
        value = evaluate(operand, row, group) if operand
        branches.each do |condition, result|
          test = evaluate(condition, row, group)
          matches = operand ? (!value.nil? && !test.nil? && Types.compare(value, test).zero?) : test == true
          return cast_value(evaluate(result, row, group), expr.type) if matches
        end
        cast_value(evaluate(fallback, row, group), expr.type)
      when :in
        charge
        value = evaluate(expr.args.first, row, group)
        choices = if expr.extra[:query]
          correlated(expr.extra[:scope], row) { select(expr.extra[:query]).rows.map(&:first) }
        else
          expr.args.drop(1).map { |a| evaluate(a, row, group) }
        end
        answer = false
        choices.each do |item|
          if item.nil? || value.nil?
            answer = nil
          elsif Types.compare(value, item).zero?
            answer = true
            break
          end
        end
        expr.extra[:negated] && !answer.nil? ? !answer : answer
      when :between
        charge
        value, low, high = expr.args.map { |e| evaluate(e, row, group) }
        answer = value.nil? || low.nil? || high.nil? ? nil : Types.compare(value, low) >= 0 && Types.compare(value, high) <= 0
        expr.extra[:negated] && !answer.nil? ? !answer : answer
      when :array
        cast_value(expr.args.map { |a| evaluate(a, row, group) }, expr.type)
      when :row then expr.args.map { |a| evaluate(a, row, group) }
      when :subquery, :exists
        rel = correlated(expr.extra[:scope], row) { select(expr.value) }
        if expr.kind == :exists
          expr.extra[:negated] ? rel.rows.empty? : !rel.rows.empty?
        else
          raise Error.new('21000', 'scalar subquery returned multiple rows') if rel.rows.length > 1
          raise Error.new('42601', 'scalar subquery must have one column') unless rel.columns.length == 1
          rel.rows.first&.first
        end
      else raise Error.new('0A000', "evaluation of #{expr.kind} is not implemented")
      end
    end

    def correlated(columns, row)
      @outer << [columns, row]
      yield
    ensure
      @outer.pop
    end

    def integer_binary(expression, a, b)
      return nil if a.nil? || b.nil?
      value = case expression.value
      when '=' then return a == b
      when '<>', '!=' then return a != b
      when '<' then return a < b
      when '<=' then return a <= b
      when '>' then return a > b
      when '>=' then return a >= b
      when '+' then a + b
      when '-' then a - b
      when '*' then a * b
      when '/', '%'
        raise Error.new('22012', 'division by zero') if b.zero?
        expression.value == '%' ? a.remainder(b) : (a.abs / b.abs) * (a.negative? != b.negative? ? -1 : 1)
      end
      bounds = expression.extra
      raise Error.new('22003', "integer out of range for #{expression.type}") unless value.between?(bounds[:minimum], bounds[:maximum])
      value
    end

    def aggregate(expr, group)
      values = []
      group.each do |row|
        next if expr.extra[:filter_expr] && evaluate(expr.extra[:filter_expr], row) != true
        charge
        values << (expr.args[0]&.kind == :star ? 1 : evaluate(expr.args[0], row))
      end
      values = values.uniq if expr.extra[:distinct]
      nonnull = values.compact
      case expr.value
      when 'count' then nonnull.length
      when 'sum'
        return nil if nonnull.empty?
        nonnull.reduce(cast_value(0, expr.type)) { |sum, v| Types.binary('+', sum, cast_value(v, expr.type), type: expr.type) }
      when 'avg'
        return nil if nonnull.empty?
        sum = nonnull.reduce(cast_value(0, expr.type)) { |s, v| Types.binary('+', s, cast_value(v, expr.type), type: expr.type) }
        Types.binary('/', sum, cast_value(nonnull.length, expr.type), type: expr.type)
      when 'min' then nonnull.min { |a, b| Types.compare(a, b) }
      when 'max' then nonnull.max { |a, b| Types.compare(a, b) }
      when 'bool_and', 'every' then nonnull.empty? ? nil : nonnull.all?
      when 'bool_or' then nonnull.empty? ? nil : nonnull.any?
      when 'array_agg' then values.empty? ? nil : cast_value(values, "#{expr.args[0].type}[]")
      when 'string_agg'
        return nil if nonnull.empty?
        delimiter = evaluate(expr.args[1], group.first)
        nonnull.join(delimiter || '')
      else raise Error.new('0A000', "aggregate #{expr.value} is not implemented")
      end
    end
  end
end
