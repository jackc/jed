# frozen_string_literal: true

module JedNative
  # These walks stay fresh even for mutable public ASTs. Scalar children cannot
  # contain syntax; avoiding their recursive calls also avoids transient arrays.
  module QueryWalks
    def local_aggregate?(value)
      return super if @native_query_walks_disabled
      if value.is_a?(Array)
        value.each do |child|
          return true if (child.is_a?(Hash) || child.is_a?(Array)) && local_aggregate?(child)
        end
      elsif value.is_a?(Hash)
        kind = value[:kind]
        return false if kind == :subquery || kind == :exists || kind == :array_query
        return true if kind == :call && Functions.aggregate?(value[:name]) && !value[:over]
        value.each_value do |child|
          return true if (child.is_a?(Hash) || child.is_a?(Array)) && local_aggregate?(child)
        end
      end
      false
    end

    def contains_aggregate?(node)
      return super if @native_query_walks_disabled
      return false unless node.is_a?(Hash)
      return true if node[:kind] == :call && Functions.aggregate?(node[:name]) && !node[:over]
      node.each_value do |child|
        if child.is_a?(Array)
          child.each { |item| return true if item.is_a?(Hash) && contains_aggregate?(item) }
        elsif child.is_a?(Hash)
          return true if contains_aggregate?(child)
        end
      end
      false
    end

    private

    def window_nodes(value, result = [])
      return super if @native_query_walks_disabled
      if value.is_a?(Array)
        value.each do |child|
          window_nodes(child, result) if child.is_a?(Hash) || child.is_a?(Array)
        end
      elsif value.is_a?(Hash)
        kind = value[:kind]
        return result if kind == :subquery || kind == :exists || kind == :array_query
        result << value if kind == :call && value[:over]
        value.each do |key, child|
          next if key == :query || key == :with
          window_nodes(child, result) if child.is_a?(Hash) || child.is_a?(Array)
        end
      end
      result
    end

    def validate_opaque_query_tree(node, ctes = {})
      return super if @native_query_walks_disabled
      case node
      when Array
        node.each do |child|
          validate_opaque_query_tree(child, ctes) if child.is_a?(Hash) || child.is_a?(Array)
        end
      when Hash
        definitions = node[:with]
        # Recursive validation only reads the scope. Copy it when this node
        # introduces bindings, preserving shadowing without a hash per AST node.
        local = if (!definitions || definitions.empty?) && ctes.instance_of?(Hash) && !ctes.default_proc
                  ctes
                else
                  ctes.merge((definitions || []).to_h { |entry| [entry[:name], true] })
                end
        kind = node[:kind]
        if kind == :table && !local[node[:name]]
          require_parsed_view(@views[node[:name]]) if @views[node[:name]]
          @views.each_value { |view| require_parsed_view(view) } if node[:name] == 'jed_columns'
        elsif kind == :insert || kind == :update || kind == :delete
          if (view = @views[node[:table]])
            require_parsed_view(view)
          elsif (table = @tables[node[:table]])
            require_parsed_write(table, node)
          end
        end
        node.each_value do |child|
          validate_opaque_query_tree(child, local) if child.is_a?(Hash) || child.is_a?(Array)
        end
      end
    end
  end
  Engine.prepend(QueryWalks)
end
