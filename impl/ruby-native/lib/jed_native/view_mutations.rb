# frozen_string_literal: true

module JedNative
  module ViewSourceBindings
    private

    def statement(node, outer = [])
      return super unless node[:kind] == :view_source
      statement(node[:query], outer) + node[:defaults].map do |name, value|
        { name: name, type: expression(value, []) }
      end
    end
  end
  ParameterBindings.prepend(ViewSourceBindings)

  module ViewMutationPlanning
    def build_query_plan(statement)
      return super unless statement[:kind] == :view_source
      root = build_query_plan(statement[:query])
      work = statement[:defaults].sum { |_, node| expression_work(plan_compile(node, [])) }
      root.cost = [QueryPlans::MAX_ESTIMATE, root.cost + root.rows * work].min
      @explain_plans[statement.object_id] = root
      root
    end

    def explain_query(statement)
      inner = statement[:statement]
      if %i[insert update delete].include?(inner[:kind]) && @views.key?(inner[:table])
        rewrite_view_mutation(inner, inner[:kind]) do |rewritten|
          super(statement.merge(statement: rewritten))
        end
      else
        super
      end
    end
  end
  Engine.prepend(ViewMutationPlanning)
end
