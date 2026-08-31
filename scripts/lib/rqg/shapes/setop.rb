# frozen_string_literal: true

require_relative "../gen"
require_relative "../expr"
require_relative "../case"

module RQG
  module Shapes
    # setop — two SELECTs over the SAME table (so the projected column types match exactly) combined
    # by UNION [ALL] / INTERSECT [ALL] / EXCEPT [ALL], each with its own WHERE.
    #
    # Half of the cases carry a trailing ORDER BY (`nosort` — the ordering itself is pinned), half
    # stay unordered (`rowsort` — the harness compares the multiset). The ordered half was
    # previously impossible: a set-op ORDER BY is column-name/ordinal-only, with no room for the
    # `COLLATE "C"` the generator used to need on a text key, so a text ordering would have
    # diverged from the en_US oracle and the shape omitted ORDER BY entirely. With the oracle on
    # builtin C.UTF-8 (oracle_profile.toml) a bare key agrees, and the coverage comes back.
    #
    # Totality: the key list is EVERY projected column, so the only ties are between rows identical
    # in all of them — indistinguishable in the output, so the rendered values are deterministic
    # whatever order the engine puts them in. That holds for the ALL variants too, where a row
    # satisfying both arms is emitted twice.
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

        if rng.rand < 0.5
          query += " ORDER BY #{order_keys(rng, proj_cols)}"
          sortmode = "nosort"
          caps |= Set[SpecData.cap(:order_by)]
          caps |= Set[SpecData.cap(:order_by_keys)] if proj_cols.size > 1
        else
          sortmode = "rowsort"
        end

        Case.new(seed: seed, shape: "setop", setup: [ddl, insert],
                 query: query, sortmode: sortmode, caps: caps)
      end

      # Every projected column, in a random order, each optionally DESC / NULLS FIRST|LAST. Names
      # only — a set-op ORDER BY may not reference an expression or a qualified column, and both
      # arms project the same names, so these resolve against the set operation's output columns.
      def order_keys(rng, proj_cols)
        proj_cols.shuffle(random: rng).map do |c|
          key = c.name
          key += " DESC" if rng.rand < 0.5
          key += rng.rand < 0.5 ? " NULLS FIRST" : " NULLS LAST" if rng.rand < 0.4
          key
        end.join(", ")
      end

      def subset(rng, cols)
        chosen = cols.select { rng.rand < 0.55 }
        chosen.empty? ? [cols[rng.rand(cols.size)]] : chosen
      end
    end

    REGISTRY["setop"] = SetOp.method(:generate)
  end
end
