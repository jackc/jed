# frozen_string_literal: true

module JedNative
  # Schema resolution happens before a table is published, even when it has no
  # rows. Runtime enforcement uses the resolved column names stored here.
  module Constraints
    def flush_foreign_keys
      @checking_foreign_keys = true
      @foreign_key_checks.each_key do |name|
        stored = @tables[name]
        next unless stored
        foreign = stored[:constraints].select { |constraint| constraint[:kind] == :foreign_key }
        validate_rows(stored.merge(constraints: foreign, indexes: []), stored[:rows])
      end
    ensure
      @checking_foreign_keys = false
    end

    def validate_definition(table)
      columns = table_columns(table)
      table[:constraints].each do |constraint|
        names = constraint[:columns]
        if names
          names.each { |name| raise Error.new('42703', "unknown column #{name}") unless columns.any? { |c| c[:name] == name } }
          raise Error.new('42701', 'column appears twice in constraint') unless names.uniq.length == names.length
        end
        next unless constraint[:kind] == :check
        schema_expression_structure(constraint[:expr], :check)
        compile_filter(constraint[:expr], columns)
      end
      table[:constraints].each do |constraint|
        resolve_foreign_key(table, constraint) if constraint[:kind] == :foreign_key
      end
      name_constraints(table)
    end

    private

    def resolve_foreign_key(child, constraint)
      parent_name = constraint[:table] || constraint[:ref_table]
      parent = parent_name == child[:name] ? child : table(parent_name)
      raise Error.new('42P16', 'foreign key crosses persistence domains') unless !!child[:temporary] == !!parent[:temporary]
      names = constraint[:ref_columns]
      unless names
        names = parent[:constraints].find { |c| c[:kind] == :primary_key }&.dig(:columns)
        raise Error.new('42704', 'referenced table has no primary key') unless names
      end
      remote = names.map do |name|
        parent[:columns].find { |c| c[:name] == name } || raise(Error.new('42703', "unknown referenced column #{name}"))
      end
      raise Error.new('42701', 'referenced column appears twice') unless names.uniq.length == names.length
      local = constraint[:columns].map { |name| child[:columns].find { |c| c[:name] == name } }
      raise Error.new('42830', 'foreign key column count differs') unless local.length == remote.length
      key = names.sort
      unique = parent[:constraints].any? { |c| %i[primary_key unique].include?(c[:kind]) && c[:columns].sort == key }
      unique ||= parent[:indexes].any? do |index|
        next false unless index[:unique] && !index[:where]
        index_names = index[:columns] || index[:elements]&.map { |e| e[:expr][:name] if e[:expr][:kind] == :column }
        index_names && index_names.none?(&:nil?) && index_names.sort == key
      end
      raise Error.new('42830', 'referenced columns have no unique constraint') unless unique
      local.zip(remote).each do |a, b|
        left, right = a[:type], b[:type]
        comparable = left == right || Types::INTEGER_BITS.key?(left) && (Types::INTEGER_BITS.key?(right) || right == 'decimal') ||
                     %w[f32 f64].include?(left) && %w[f32 f64].include?(right)
        raise Error.new('42804', 'foreign key columns have incompatible types') unless comparable
      end
      raise Error.new('0A000', 'MATCH PARTIAL is not implemented') if constraint[:match] == :partial
      raise Error.new('42601', 'invalid MATCH type') unless [nil, :simple, :full].include?(constraint[:match])
      %i[on_delete on_update].each do |action|
        unless [nil, :no_action, :restrict, :cascade, :set_null, :set_default].include?(constraint[action])
          raise Error.new('42601', 'invalid referential action')
        end
      end
      constraint[:ref_columns] = names.dup
    end

    def name_constraints(table)
      explicit = table[:constraints].filter_map { |c| c[:name] }
      raise Error.new('42710', 'duplicate constraint name') unless explicit.uniq.length == explicit.length
      local = []
      used = @tables.values.reject { |t| t[:name] == table[:name] }.flat_map { |t| t[:constraints].filter_map { |c| c[:name] } }
      table[:constraints].each do |constraint|
        if constraint[:name]
          raise Error.new('42710', 'duplicate constraint name') if local.include?(constraint[:name])
          local << constraint[:name]
          used << constraint[:name]
          next
        end
        suffix = { primary_key: 'pkey', unique: 'key', check: 'check', foreign_key: 'fkey', exclude: 'excl' }.fetch(constraint[:kind])
        names = constraint[:kind] == :primary_key ? [] : constraint[:columns]
        names ||= schema_column_names(constraint[:elements] || constraint[:expr]).take(1)
        base = ([table[:name]] + names + [suffix]).join('_')
        name, sequence = identifier_prefix(base, 63), 0
        while used.include?(name) || %i[unique exclude].include?(constraint[:kind]) && relation_exists?(name)
          sequence += 1
          ending = sequence.to_s
          name = identifier_prefix(base, 63 - ending.bytesize) + ending
        end
        constraint[:name] = name
        used << name
        local << name
      end
    end

    def identifier_prefix(value, bytes)
      result = value.byteslice(0, bytes)
      result = result.byteslice(0, result.bytesize - 1) until result.valid_encoding?
      result
    end

    def schema_column_names(node)
      case node
      when Array then node.flat_map { |child| schema_column_names(child) }.uniq
      when Hash
        node[:kind] == :column ? [node[:name]] : node.values.flat_map { |child| schema_column_names(child) }.uniq
      else []
      end
    end

    def validate_index_expression(node, columns)
      schema_expression_structure(node, :index)
      validate_index_dependencies(compile(node, columns))
    end

    def validate_index_dependencies(expression)
      visit_index_children(expression.args)
      visit_index_children(expression.extra)
      case expression.kind
      when :jsonpath_function
        reject_index_dependency unless JsonPathFunctions.volatility(expression.value) == 'immutable'
      when :call
        name, types = expression.value, expression.args.map(&:type)
        entries = FunctionCatalog::VOLATILITY[name]
        if entries
          signature = Functions.resolve_signature(name, types)
          index = FunctionCatalog::SIGNATURES[name].index(signature)
          volatility = entries[index][1]
          epoch = name == 'date_part' && expression.args[0].kind == :literal && expression.args[0].value.to_s.downcase == 'epoch'
          reject_index_dependency unless volatility == 'immutable' || epoch
        end
        if %w[to_json to_jsonb array_to_json row_to_json json_scalar json_build_array jsonb_build_array json_build_object jsonb_build_object].include?(name)
          reject_index_dependency if types.any? { |type| type_contains_timestamptz?(type) }
        end
        if name == 'date_part' && types[1] == 'timestamptz'
          field = expression.args[0]
          reject_index_dependency unless field.kind == :literal && field.value.to_s.downcase == 'epoch'
        end
      when :cast
        source = expression.args[0]
        target = expression.type
        crosses_zone = (source.type == 'timestamptz' && %w[date timestamp text].include?(target)) ||
                       (%w[date timestamp].include?(source.type) && target == 'timestamptz') ||
                       (target == 'text' && type_contains_timestamptz?(source.type))
        runtime_date = source.type == 'text' && %w[date timestamp timestamptz].include?(target) && source.kind != :literal
        reject_index_dependency if crosses_zone || runtime_date
      end
    end

    def visit_index_children(value)
      case value
      when Expression then validate_index_dependencies(value)
      when Array then value.each { |child| visit_index_children(child) }
      when Hash then value.each_value { |child| visit_index_children(child) }
      end
    end

    def type_contains_timestamptz?(type, seen = {})
      return true if type == 'timestamptz' || type == 'tstzrange'
      return type_contains_timestamptz?(type[0...-2], seen) if type.end_with?('[]')
      return false if seen[type]
      seen[type] = true
      (@types[type] || []).any? { |field| type_contains_timestamptz?(field[:type], seen) }
    end

    def reject_index_dependency
      raise Error.new('42P17', 'index expression depends on session, clock, entropy, or state')
    end

    def schema_expression_structure(node, context)
      case node
      when Array then node.each { |child| schema_expression_structure(child, context) }
      when Hash
        case node[:kind]
        when :subquery, :exists, :array_query then raise Error.new('0A000', "subquery in #{context}")
        when :param then raise Error.new('42P02', 'parameter in schema expression')
        when :call
          raise Error.new('42P20', "window function in #{context}") if node[:over]
          raise Error.new('42803', "aggregate in #{context}") if Functions.aggregate?(node[:name])
        end
        node.each_value { |child| schema_expression_structure(child, context) }
      end
    end
  end
  Engine.prepend(Constraints)
end
