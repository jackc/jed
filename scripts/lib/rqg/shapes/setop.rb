# frozen_string_literal: true

require_relative "../gen"
require_relative "../expr"
require_relative "../case"

module RQG
  module Shapes
    # setop — two SELECTs over the SAME table (so the projected column types match exactly) combined
    # by UNION [ALL] / INTERSECT [ALL] / EXCEPT [ALL], each with its own WHERE. Always rowsort, never
    # ORDER BY: set-op dedup/keep is multiset equality (deterministic collation = byte-equal, so it
    # agrees C-vs-en_US), and the harness compares the multiset order-insensitively — which sidesteps
    # the one hazard (a set-op ORDER BY is column/ordinal-only, where a text key can't take COLLATE
    # "C" and would diverge). Projecting only the columns avoids any text-ordering path entirely.
    module SetOp
      module_function

      OPS = { "UNION" => :union, "UNION ALL" => :union, "INTERSECT" => :intersect,
              "INTERSECT ALL" => :intersect, "EXCEPT" => :except, "EXCEPT ALL" => :except }.freeze

      def generate(seed)
        rng = Random.new(seed)
        table = Schema.gen(rng, "u#{seed}")
        ddl, ddl_caps = Schema.ddl(table)
        insert = "INSERT INTO #{table.name} VALUES " \
                 "#{Data.rows(rng, table, rng.rand(6..12)).map { |r| "(#{r.join(', ')})" }.join(', ')}"

        # Both arms project the SAME column list (identical types) — equality dedup is collation-safe.
        proj_cols = subset(rng, table.columns)
        cols = proj_cols.map(&:name).join(", ")
        op = ["UNION", "UNION ALL", "INTERSECT", "INTERSECT ALL", "EXCEPT", "EXCEPT ALL"][rng.rand(6)]

        c1 = Ctx.new(rng, RQG.col_refs(table))
        c2 = Ctx.new(rng, RQG.col_refs(table))
        left = "SELECT #{cols} FROM #{table.name} WHERE #{Expr.predicate(c1, rng.rand(1..2))}"
        right = "SELECT #{cols} FROM #{table.name} WHERE #{Expr.predicate(c2, rng.rand(1..2))}"
        query = "#{left} #{op} #{right}"

        caps = ddl_caps | Set[SpecData.cap(:insert), SpecData.cap(:insert_multi_row),
                              SpecData.cap(:select), SpecData.cap(OPS[op])] | c1.caps | c2.caps
        Case.new(seed: seed, shape: "setop", setup: [ddl, insert],
                 query: query, sortmode: "rowsort", caps: caps)
      end

      def subset(rng, cols)
        chosen = cols.select { rng.rand < 0.55 }
        chosen.empty? ? [cols[rng.rand(cols.size)]] : chosen
      end
    end

    REGISTRY["setop"] = SetOp.method(:generate)
  end
end
