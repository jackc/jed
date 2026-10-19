# frozen_string_literal: true

module JedNative
  # A host-owned envelope, never an SQL privilege catalog. Preflight is static:
  # even an empty scan checks every authored/catalog expression it would apply.
  module Capabilities
    TABLE_PRIVILEGES = %i[select insert update delete].freeze
    attr_accessor :allow_ddl, :allow_temp_ddl
    attr_reader :default_privileges

    def initialize(allow_ddl: true, allow_temp_ddl: nil, default_privileges: TABLE_PRIVILEGES, **options)
      @allow_ddl, @allow_temp_ddl = allow_ddl, allow_temp_ddl
      self.default_privileges = default_privileges
      @privilege_grants, @privilege_revokes = {}, {}
      super(**options)
    end

    def default_privileges=(privileges)
      @default_privileges = normalize_privileges(privileges) & TABLE_PRIVILEGES
    end

    def grant(privileges, object)
      @privilege_grants[String(object)] = ((@privilege_grants[String(object)] || []) + normalize_privileges(privileges)).uniq
    end

    def revoke(privileges, object)
      @privilege_revokes[String(object)] = ((@privilege_revokes[String(object)] || []) + normalize_privileges(privileges)).uniq
    end

    def reset_privileges
      @privilege_grants.clear
      @privilege_revokes.clear
    end

    def unrestricted_capabilities?
      @allow_ddl && @allow_temp_ddl != false && @default_privileges.sort == TABLE_PRIVILEGES.sort &&
        @privilege_revokes.empty? && !@read_only
    end

    def dispatch(statement)
      owns = !@capability_preflight
      if owns
        @capability_preflight = true
        unless unrestricted_capabilities?
          authorize_node(statement, {}, true, [])
        end
      end
      super
    ensure
      @capability_preflight = false if owns
    end

    private

    def normalize_privileges(privileges)
      values = privileges.is_a?(String) ? privileges.split(/[\s,]+/) : Array(privileges)
      values = values.flat_map { |value| value.to_s.casecmp?('all') ? TABLE_PRIVILEGES : [value.to_s.downcase.to_sym] }
      values.reject! { |value| value == :none }
      raise Error.new('22023', 'unknown privilege') unless (values - TABLE_PRIVILEGES - [:execute]).empty?
      values.uniq
    end

    def require_privilege(name, operation, function: false)
      public_name = local_domain_name(name)
      defaults = function ? [:execute] : @default_privileges
      grants = @privilege_grants[public_name] || []
      revoked = @privilege_revokes[public_name] || []
      return if (defaults.include?(operation) || grants.include?(operation)) && !revoked.include?(operation)
      raise Error.new('42501', "permission denied for #{function ? 'function' : 'table'} #{public_name}")
    end

    def authorize_node(node, ctes, tables, seen_views)
      case node
      when Array
        node.each { |child| authorize_node(child, ctes, tables, seen_views) }
      when Hash
        kind = node[:kind]
        ctes = ctes.dup
        (node[:with] || []).each do |cte|
          ctes[cte[:name]] = true if node[:recursive]
          authorize_node(cte[:query], ctes, tables, seen_views)
          ctes[cte[:name]] = true
        end
        if kind == :select && node[:into]
          authorize_ddl(node[:into].merge(kind: :create_table))
        elsif kind.to_s.start_with?('create_', 'alter_', 'drop_') || %i[truncate analyze reindex].include?(kind)
          authorize_ddl(node)
          if %i[alter_view alter_table].include?(kind) && (view = @views[node[:name]])
            changes_definer = (node[:actions] || []).any? do |action|
              action[:kind] == :reset_options && action[:options].include?('security_invoker') ||
                action[:kind] == :set_options && action[:options].key?('security_invoker') && !view_boolean_option(action[:options]['security_invoker'])
            end
            authorize_node(view[:query], {}, true, seen_views + [node[:name]]) if changes_definer
          end
          return if kind == :create_table && node[:query] && node[:with_no_data]
        end
        if kind == :table
          name = node[:name]
          unless !node[:database] && ctes[name]
            view = @views[name]
            exists = @tables.key?(name) || view || introspection_relation?(name)
            require_privilege(name, :select) if tables && exists
            if view && !seen_views.include?(name)
              authorize_node(view[:query], {}, view_boolean_option(view.dig(:options, 'security_invoker')), seen_views + [name])
            end
          end
        elsif %i[insert update delete].include?(kind)
          authorize_mutation(node, ctes, tables, seen_views)
        elsif kind == :analyze
          require_privilege(node[:table], :select) if @tables.key?(node[:table])
        elsif kind == :reindex && %i[table index].include?(node[:object])
          owner = node[:object] == :table ? @tables[node[:name]] : @tables.values.find { |table| table[:indexes].any? { |index| index[:name] == node[:name] } }
          require_privilege(owner[:name], :select) if owner
        end
        function = %i[call function srf].include?(kind) ? node[:name] : nil
        function ||= kind.to_s if FunctionCatalog::SIGNATURES.key?(kind.to_s)
        if function
          require_privilege(function, :execute, function: true) if FunctionCatalog::SIGNATURES.key?(function) || Functions::AGGREGATES.include?(function) || Functions::WINDOWS.include?(function)
          if @read_only && %w[nextval setval].include?(function)
            argument = node[:args]&.first
            name = argument && argument[:kind] == :literal && argument[:value]
            sequence = name.is_a?(String) && @sequences[sequence_name(name)]
            temp = sequence && (sequence[:temporary] || @tables[sequence[:owned_by]&.first]&.dig(:temporary))
            raise Error.new('25006', 'read-only transaction') unless temp
          end
        end
        node.each do |key, child|
          next if key == :with
          authorize_node(child, ctes, tables, seen_views)
        end
      end
    end

    def authorize_ddl(statement)
      maintenance = %i[analyze reindex].include?(statement[:kind])
      temporary = !maintenance && temporary_statement?(statement)
      allowed = temporary && !@allow_temp_ddl.nil? ? @allow_temp_ddl : @allow_ddl
      raise Error.new('42501', 'schema changes are not permitted') unless allowed
    end

    def authorize_mutation(statement, ctes, tables, seen_views)
      kind, name = statement[:kind], statement[:table]
      view = @views[name]
      stored = @tables[name]
      return unless stored || view
      require_privilege(name, kind) if tables
      conflict = statement[:on_conflict]
      require_privilege(name, :update) if tables && conflict && conflict[:action] == :update
      aliases = [statement[:alias] || local_domain_name(name), 'old', 'new'] + (statement[:returning_aliases] || {}).values
      reads = target_columns_read?([statement[:where], statement[:assignments], statement[:returning]], aliases)
      reads ||= conflict && (conflict[:columns] || conflict[:constraint] || target_columns_read?([conflict[:where], conflict[:assignments]], aliases + ['excluded']))
      require_privilege(name, :select) if tables && reads
      plan = view && view_write_plan(view, kind)
      if plan
        authorize_invoker_write(view, kind, reads) if tables
        stored = table(plan[:table])
        authorize_node(view[:query], {}, false, seen_views + [name])
      end
      return if kind == :delete
      stored[:constraints].each { |constraint| authorize_node(constraint[:expr], ctes, false, seen_views) if constraint[:kind] == :check }
      visible = view ? view_schema(view) : stored[:columns]
      mapping = plan ? plan[:writable] : visible.to_h { |column| [column[:name], column[:name]] }
      defaults = plan ? plan[:defaults] : {}
      if kind == :insert
        specified = statement[:columns] || (statement[:default_values] ? [] : visible.map { |column| column[:name] })
        supplied = specified.filter_map { |column| mapping[column] }
        stored[:columns].each do |column|
          source = specified.find { |candidate| mapping[candidate] == column[:name] }
          ordinal = source && specified.index(source)
          omitted = !supplied.include?(column[:name])
          explicit = ordinal && (statement[:rows] || []).any? { |row| row[ordinal]&.dig(:kind) == :default }
          if omitted || explicit || statement[:default_values]
            expression = source && (view&.dig(:defaults, source) || defaults[source]) || column[:default]
            authorize_node(expression, ctes, false, seen_views)
          end
        end
      end
      assignments = (statement[:assignments] || []) + (conflict&.dig(:assignments) || [])
      assignments.each do |assignment|
        next unless assignment.dig(:expr, :kind) == :default
        local_name = assignment[:column]
        column = stored[:columns].find { |candidate| candidate[:name] == mapping[local_name] }
        expression = view&.dig(:defaults, local_name) || defaults[local_name] || column&.dig(:default)
        authorize_node(expression, ctes, false, seen_views)
      end
    end

    def view_boolean_option(value) = value == true || %w[true on yes 1].include?(value.to_s.downcase)

    def authorize_invoker_write(view, operation, reads)
      return unless view_boolean_option(view.dig(:options, 'security_invoker'))
      source = view[:query][:from]&.first
      return unless source && source[:kind] == :table
      name = source[:name]
      require_privilege(name, operation)
      reads ||= operation != :insert && target_columns_read?(view[:query][:where], [source[:alias] || name])
      require_privilege(name, :select) if reads
      authorize_invoker_write(@views[name], operation, reads) if @views[name]
    end

    def target_columns_read?(node, aliases)
      case node
      when Array then node.any? { |child| target_columns_read?(child, aliases) }
      when Hash
        return false if %i[select values setop set_operation subquery exists array_query].include?(node[:kind])
        return !node[:table] || aliases.include?(node[:table]) if %i[column star].include?(node[:kind])
        node.any? { |_, child| target_columns_read?(child, aliases) }
      else false
      end
    end
  end

  Engine.prepend(Capabilities)
end
