# frozen_string_literal: true

require_relative "../gen"
require_relative "../expr"
require_relative "../case"

module RQG
  module Shapes
    # select_where — the Phase-1 surface: a random schema + data, a type-aware WHERE expression tree,
    # a projection, and either a total ORDER BY (nosort, optional LIMIT/OFFSET) or no ORDER BY
    # (rowsort), with an occasional DISTINCT. Everything stays in the PG∩jed-agreeing subset by
    # construction (Expr's family gating + the COLLATE "C" chokepoint).
    module SelectWhere
      module_function

      def generate(seed)
        rng = Random.new(seed)
        table = Schema.gen(rng, "t#{seed}")
        ddl, ddl_caps = Schema.ddl(table)
        nrows = rng.rand(4..10)
        rows = Data.rows(rng, table, nrows)
        insert = "INSERT INTO #{table.name} VALUES #{rows.map { |r| "(#{r.join(', ')})" }.join(', ')}"
        insert_caps = Set[SpecData.cap(:insert)]
        insert_caps << SpecData.cap(:insert_multi_row) if nrows > 1

        ctx = Ctx.new(rng, RQG.col_refs(table))
        where = Expr.predicate(ctx, rng.rand(1..3))
        proj, sortmode, tail = select_tail(ctx, table)
        query = "SELECT #{proj} FROM #{table.name} WHERE #{where}#{tail}"

        caps = ddl_caps | insert_caps | ctx.caps | Set[SpecData.cap(:select)]
        Case.new(seed: seed, shape: "select_where", setup: [ddl, insert],
                 query: query, sortmode: sortmode, caps: caps)
      end

      # [projection, sortmode, "...ORDER BY/LIMIT tail"]. DISTINCT and no-ORDER-BY use rowsort; a
      # total ORDER BY ending in the (unique) PK is nosort and may carry LIMIT/OFFSET.
      def select_tail(ctx, table)
        if ctx.chance(0.25)
          ctx.use(:distinct)
          return ["DISTINCT #{subset(ctx, table.columns).map(&:name).join(', ')}", "rowsort", ""]
        end

        proj = projection(ctx, table)
        return [proj, "rowsort", ""] unless ctx.chance(0.70)

        ctx.use(:order_by)
        keys = order_keys(ctx, table)
        tail = " ORDER BY #{keys.join(', ')}"
        if ctx.chance(0.40)
          ctx.use(:limit)
          tail += " LIMIT #{ctx.rand(0..8)}"
          (ctx.use(:offset); tail += " OFFSET #{ctx.rand(0..4)}") if ctx.chance(0.5)
        end
        [proj, "nosort", tail]
      end

      def projection(ctx, table)
        if ctx.chance(0.5)
          ctx.use(:select_star)
          return "*"
        end
        subset(ctx, table.columns).map do |c|
          if ctx.chance(0.2)
            ctx.use(:column_alias)
            "#{c.name} AS #{c.name}_x"
          else
            c.name
          end
        end.join(", ")
      end

      # 0-2 random sort keys (text collated, optional DESC / NULLS) then the PK as a total-order
      # tiebreaker — so the result order is fully deterministic and agrees with PG (nosort).
      def order_keys(ctx, table)
        extra = subset(ctx, table.columns.reject(&:pk), allow_empty: true, max: 2)
        keys = extra.map do |c|
          ctx.use(:order_by_keys)
          key = c.name
          if RQG.family(c.type) == :text
            ctx.use(:collate)
            key += ' COLLATE "C"'
          end
          key += " DESC" if ctx.chance(0.5)
          key += ctx.chance(0.5) ? " NULLS FIRST" : " NULLS LAST" if ctx.chance(0.4)
          key
        end
        keys << table.pk.name
        keys
      end

      # A random subset of columns, order preserved. Non-empty unless allow_empty.
      def subset(ctx, cols, allow_empty: false, max: nil)
        chosen = cols.select { ctx.chance(0.55) }
        chosen = [ctx.pick(cols)] if chosen.empty? && !allow_empty && !cols.empty?
        chosen = chosen.first(max) if max
        chosen
      end
    end

    REGISTRY["select_where"] = SelectWhere.method(:generate)
  end
end
