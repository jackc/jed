# frozen_string_literal: true

module JedNative
  # Registered Ruby code is an explicit host boundary. SQL can call these
  # functions, but cannot register code or obtain arbitrary Ruby dispatch.
  class HostFunction
    attr_reader :name, :args, :result, :cost, :volatility, :cross_core,
                :component_id, :semantic_version, :kernel, :batch_kernel
    def initialize(name, args:, returns:, cost: 1, volatility: :volatile,
                   cross_core: false, component_id: nil, semantic_version: 0,
                   kernel: nil, batch_kernel: nil, &block)
      @name = String(name).dup.freeze
      @args = args.map { |type| Types.normalize(type).freeze }.freeze
      @result = Types.normalize(returns).freeze
      unless (@args + [@result]).all? { |type| FileFormat::TYPECODE.key?(type) }
        raise Error.new('42704', 'host functions require existing scalar types')
      end
      raise Error.new('22023', 'host function cost must be nonnegative') unless cost.is_a?(Integer) && cost >= 0
      raise Error.new('22023', 'invalid host function volatility') unless %i[immutable stable volatile].include?(volatility)
      raise Error.new('22023', 'semantic version must fit u32') unless semantic_version.is_a?(Integer) && semantic_version.between?(0, (1 << 32) - 1)
      @kernel, @batch_kernel = kernel || block, batch_kernel
      raise Error.new('22023', 'supply one scalar or batch kernel') unless !!@kernel ^ !!@batch_kernel
      @cost, @volatility, @cross_core = cost, volatility, !!cross_core
      @component_id = component_id && String(component_id).dup.freeze
      @semantic_version = semantic_version
      freeze
    end
    def dependency
      { name: @name, arg_types: @args.map { |type| FileFormat::TYPECODE.fetch(type) },
        result: FileFormat::TYPECODE.fetch(@result), component_id: @component_id,
        semantic_version: @semantic_version }
    end
    def call(arguments)
      arguments = arguments.map { |value| HostValues.copy(value) }
      return @kernel.call(*arguments) if @kernel
      out = []
      begin
        @batch_kernel.call(arguments.map { |value| [value] }, out)
      rescue StandardError
        raise Error.new('22000', 'batch kernel error does not identify a failing row') unless out.empty?
        raise
      end
      raise Error.new('22000', 'batch kernel must append one result per row') unless out.length == 1
      out.first
    end
  end

  module HostEnvironment
    def register_function(function = nil, name: nil, **options, &block)
      function ||= HostFunction.new(name, **options, &block)
      raise Error.new('22023', 'expected HostFunction') unless function.is_a?(HostFunction)
      if @functions.any? { |entry| entry.name == function.name && entry.args == function.args }
        raise Error.new('42723', 'host function signature is already registered')
      end
      with(functions: @functions + [function])
    end
    alias with_function register_function
  end
  Environment.include(HostEnvironment)

  module HostFunctionBindings
    def function(node, scope, wanted)
      return super unless @engine.host_function_name?(node[:name])
      types = (node[:args] || []).map { |arg| expression(arg, scope) }
      return super if @engine.builtin_function_signature?(node[:name], types)
      entry = @engine.resolve_host_function(node[:name], types)
      (node[:args] || []).each_with_index do |arg, index|
        expression(arg, scope, entry.args[index]) if arg[:kind] == :param
      end
      entry.result
    end
  end
  ParameterBindings.prepend(HostFunctionBindings)

  module HostFunctions
    def host_function_name?(name) = @environment && @environment.functions.any? { |entry| entry.name == name }
    def builtin_function_name?(name)
      FunctionCatalog::SIGNATURES.key?(name) || Functions.aggregate?(name) ||
        Functions::WINDOWS.include?(name) || Types::RANGES.key?(Types.normalize(name)) ||
        %w[nextval currval setval lastval coalesce greatest least nullif extract grouping
           mode percentile_cont percentile_disc timezone date_trunc mod div gcd lcm width_bucket json_object_agg jsonb_object_agg].include?(name)
    end
    def builtin_function_signature?(name, types)
      !!Functions.resolve_signature(name, types)
    rescue Error => error
      raise unless error.sqlstate == '42883'
      false
    end
    def resolve_host_function(name, types)
      candidates = (@environment&.functions || []).select do |entry|
        entry.name == name && entry.args.length == types.length && entry.args.zip(types).all? { |expected, actual| expected == actual || %w[unknown null].include?(actual) }
      end
      raise Error.new('42883', "function #{name}(#{types.join(', ')}) does not exist") if candidates.empty?
      raise Error.new('42725', "function #{name} is ambiguous") if candidates.length > 1
      candidates.first
    end
    def compile(node, columns)
      unless node[:kind] == :call && host_function_name?(node[:name])
        resolved = super
        if node[:kind] == :call && resolved.kind == :call && !builtin_function_name?(node[:name])
          raise Error.new('42883', "function #{node[:name]} does not exist")
        end
        return resolved
      end
      arguments = (node[:args] || []).map { |arg| compile(arg, columns) }
      return super if builtin_function_signature?(node[:name], arguments.map(&:type))
      function = resolve_host_function(node[:name], arguments.map(&:type))
      if node[:over] || node[:filter] || node[:distinct] || node[:within_group] || !(node[:order_by] || []).empty?
        raise Error.new('42809', 'host scalar function is not an aggregate or window function')
      end
      arguments.each_with_index do |argument, index|
        argument.type = function.args[index] if %w[unknown null].include?(argument.type)
      end
      Expression.new(kind: :host_call, type: function.result, value: function, args: arguments)
    end
    def evaluate(expression, row, group = nil)
      return super unless expression.kind == :host_call
      function = expression.value
      charge(function.cost)
      outcome = host_batch_outcome(expression, row) if @host_batch_stack
      if outcome
        raise outcome[1] if outcome[0]
        value = outcome[1]
      else
        values = expression.args.map { |argument| evaluate(argument, row, group) }
        return nil if values.any?(&:nil?)
        value = function.call(values)
      end
      return nil if value.nil?
      type = function.result
      kind = Types.infer(value)
      valid = if Types::INTEGER_BITS.key?(type)
        value.is_a?(Integer)
      elsif %w[f32 f64].include?(type)
        value.is_a?(Float)
      else
        kind == type
      end
      raise Error.new('22000', "host function #{function.name} returned #{kind}, expected #{type}") unless valid
      value = Types.check_integer(value, type) if Types::INTEGER_BITS.key?(type)
      value = cast_value(value, type, source_type: type) if type == 'f32'
      HostValues.copy(value)
    end
    def expression_work(expression)
      return super unless expression.is_a?(Expression) && expression.kind == :host_call
      expression.value.cost + expression.args.sum { |argument| expression_work(argument) }
    end
    def plan_expression(expression)
      return super unless expression.kind == :host_call
      "#{expression.value.name}(#{expression.args.map { |argument| plan_expression(argument) }.join(', ')})"
    end
    def authorize_node(node, *arguments)
      if node.is_a?(Hash) && %i[call function srf].include?(node[:kind]) && host_function_name?(node[:name])
        require_privilege(node[:name], :execute, function: true)
      end
      super
    end
    def cacheable_expression?(node)
      return false if node && node[:kind] == :call && host_function_name?(node[:name])
      super
    end
    def constant_expression?(expression)
      return false if expression.is_a?(Expression) && expression.kind == :host_call
      super
    end
    def validate_index_dependencies(expression)
      if expression.kind == :host_call
        function = expression.value
        unless function.volatility == :immutable && function.component_id
          raise Error.new('42P17', 'index host functions must be immutable and declare a component identity')
        end
      end
      super
    end
    def host_dependencies(expression, check: false)
      result, stack = [], [expression]
      until stack.empty?
        item = stack.pop
        case item
        when Expression
          if item.kind == :host_call
            function = item.value
            raise Error.new('42P17', 'CHECK host functions must declare a component identity') if check && !function.component_id
            result << function.dependency
          end
          stack << item.args << item.extra
        when Array then item.each { |child| stack << child }
        when Hash then item.each_value { |child| stack << child }
        end
      end
      result.uniq.sort_by { |dependency| [dependency[:name].b, dependency[:arg_types]] }
    end
    def index_host_dependencies(index, scope)
      roots = (index[:elements] || []).map { |element| element[:expr] } + [index[:where]].compact
      roots.flat_map do |node|
        expression = compile(node, scope)
        validate_index_dependencies(expression)
        host_dependencies(expression)
      end.uniq.sort_by { |dependency| [dependency[:name].b, dependency[:arg_types]] }
    end
    def verify_host_dependencies(dependencies, object)
      (dependencies || []).each do |dependency|
        types = dependency[:arg_types].map { |code| FileFormat::CODETYPE.fetch(code) }
        function = resolve_host_function(dependency[:name], types)
        unless function.dependency == dependency
          raise Error.new('XX002', "#{object} host function #{dependency[:name]} component/version changed; rebuild required")
        end
      end
    end
    def host_index_usable?(index)
      verify_host_dependencies(index[:host_deps] || index.dig(:storage_index, :host_deps), "index #{index[:name]}")
      true
    rescue Error => error
      raise unless %w[42883 XX002].include?(error.sqlstate)
      false
    end
    def refresh_primary_indexes
      skipped = {}
      @tables = @tables.transform_values do |table|
        indexes = table[:indexes]
        usable = indexes.select { |index| host_index_usable?(index) }
        next table if usable.length == indexes.length
        skipped[table[:name]] = indexes
        table.merge(indexes: usable)
      end
      super
    ensure
      skipped&.each { |name, indexes| @tables[name] = @tables[name].merge(indexes: indexes) if @tables[name] }
    end
    def create_index(statement)
      stored = table(statement[:table])
      dependencies = index_host_dependencies(statement, table_columns(stored))
      super(statement.merge(host_deps: dependencies))
    end
    def alter_table(statement)
      previous = @host_alter_checks
      original = @tables[statement[:name]]
      @host_alter_checks = if original
        { original: original[:constraints].select { |item| item[:kind] == :check }.map { |item| item[:name] },
          retype: (statement[:actions] || []).any? { |action| action[:kind] == :set_type } }
      end
      super
    ensure
      @host_alter_checks = previous
    end
    def validate_definition(table)
      original = table[:constraints]
      # Existing checks are resolved only by an operation which evaluates them.
      # Retyping validates every surviving row and therefore records new pins.
      selected = original.reject do |constraint|
        @host_alter_checks && !@host_alter_checks[:retype] && constraint[:kind] == :check &&
          constraint[:host_deps] && !constraint[:host_deps].empty? &&
          @host_alter_checks[:original].include?(constraint[:name])
      end
      if @host_alter_checks
        super(table.merge(constraints: selected))
      else
        super
        selected = table[:constraints]
      end
      scope = table_columns(table)
      selected.each do |constraint|
        next unless constraint[:kind] == :check
        dependencies = host_dependencies(compile(constraint[:expr], scope), check: true)
        if constraint.key?(:host_deps) && !@host_alter_checks&.dig(:retype)
          verify_host_dependencies(constraint[:host_deps], "check #{constraint[:name]}")
        else
          constraint[:host_deps] = dependencies
        end
      end
    end
    def validate_rows(table, rows)
      if @host_cascade_deletes&.include?(table[:name])
        return super(table.merge(constraints: table[:constraints].reject { |item| item[:kind] == :check }), rows)
      end
      scope = table_columns(table)
      table[:constraints].each do |constraint|
        next unless constraint[:kind] == :check
        verify_host_dependencies(constraint[:host_deps], "check #{constraint[:name]}")
        actual = host_dependencies(compile(constraint[:expr], scope), check: true)
        if constraint[:host_deps] && actual != constraint[:host_deps]
          raise Error.new('XX002', 'CHECK resolved host dependency changed')
        end
      end
      table[:indexes].each { |index| verify_host_dependencies(index[:host_deps], "index #{index[:name]}") }
      super
    end
    def dispatch(statement)
      if %i[insert update delete].include?(statement[:kind]) && (stored = @tables[statement[:table]])
        if statement[:kind] != :delete
          stored[:constraints].sort_by { |constraint| constraint[:name].to_s.b }.each do |constraint|
            verify_host_dependencies(constraint[:host_deps], "check #{constraint[:name]}") if constraint[:kind] == :check
          end
        end
        stored[:indexes].each { |index| verify_host_dependencies(index[:host_deps], "index #{index[:name]}") }
      end
      super
    end
    def mutation_checks(table)
      table[:constraints].select { |item| item[:kind] == :check }.sort_by { |item| item[:name].to_s.b }.each do |item|
        verify_host_dependencies(item[:host_deps], "check #{item[:name]}")
      end
      super
    end
    def enforce_references(parent, changes, operation, visited = {})
      previous = @host_cascade_deletes
      @host_cascade_deletes = []
      unless changes.empty?
        @tables.each_value do |child|
          child[:constraints].each do |foreign|
            next unless foreign[:kind] == :foreign_key && foreign[:table] == parent[:name]
            names = foreign[:ref_columns] || parent[:constraints].find { |item| item[:kind] == :primary_key }&.dig(:columns)
            next unless names
            ordinals = names.map { |name| parent[:columns].index { |column| column[:name] == name } }
            next unless changes.any? { |before, after| !after || ordinals.any? { |ordinal| before[ordinal] != after[ordinal] } }
            action = foreign[operation == :delete ? :on_delete : :on_update]
            if operation == :delete && action == :cascade
              @host_cascade_deletes << child[:name]
            elsif %i[cascade set_null set_default].include?(action)
              mutation_checks(child)
              if action == :set_default
                foreign[:columns].each do |name|
                  column = child[:columns].find { |item| item[:name] == name }
                  compile(column[:default], []) if column[:default]
                end
              end
            end
          end
        end
      end
      super
    ensure
      @host_cascade_deletes = previous
    end
    def reindex(statement)
      reindex_targets(statement).each do |stored, indexes|
        replacements = indexes.to_h do |index|
          [index[:name], index.merge(host_deps: index_host_dependencies(index, table_columns(stored)))]
        end
        @tables[stored[:name]] = stored.merge(indexes: stored[:indexes].map { |index| replacements[index[:name]] || index })
      end
      super
    end
  end
  Engine.prepend(HostFunctions)
end
