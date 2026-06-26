# frozen_string_literal: true

require_relative "gen"
require_relative "expr"
require_relative "case"

module RQG
  # The shape registry: name -> generate(seed) -> Case. Each shape file registers itself, so adding a
  # query shape (later phases: join, group_by, subquery, setop, window, cte) is a drop-in file + one
  # require below — no edit to the driver.
  module Shapes
    REGISTRY = {}
  end
end

require_relative "shapes/select_where"
require_relative "shapes/join"
require_relative "shapes/group_by"
require_relative "shapes/subquery"
require_relative "shapes/setop"
