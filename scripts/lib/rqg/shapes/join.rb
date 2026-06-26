# frozen_string_literal: true

require_relative "../gen"
require_relative "../expr"
require_relative "../case"

module RQG
  module Shapes
    # join — two aliased tables joined INNER (ON a.id = b.r, an integer FK-like ref) or CROSS, with a
    # WHERE predicate over the combined qualified columns, a qualified/`*` projection, and either a
    # total ORDER BY (a.id, b.id — the unique pair key → nosort, optional LIMIT/OFFSET) or rowsort.
    # Integer join keys agree in PG and jed; text in WHERE/ORDER BY routes through the COLLATE "C"
    # chokepoint as in select_where.
    module SelectJoin
      module_function

      def generate(seed)
        rng = Random.new(seed)
        ta = Schema.gen(rng, "ja#{seed}")
        tb_base = Schema.gen(rng, "jb#{seed}")
        key_type = ta.pk.type
        tb = Table.new(tb_base.name, tb_base.columns + [Column.new("r", key_type, true, false)])
        na = rng.rand(4..7)
        nb = rng.rand(4..7)

        ddl_a, caps_a = Schema.ddl(ta)
        ddl_b, caps_b = Schema.ddl(tb)
        insert_a = insert(ta, Data.rows(rng, ta, na))
        insert_b = insert(tb, b_rows(rng, tb_base, nb, na))

        ctx = Ctx.new(rng, RQG.col_refs(ta, "a") + RQG.col_refs(tb, "b"))
        inner = ctx.chance(0.6)
        from = if inner
                 ctx.use(:join_inner)
                 "#{ta.name} a JOIN #{tb.name} b ON a.id = b.r"
               else
                 ctx.use(:cross_join)
                 "#{ta.name} a CROSS JOIN #{tb.name} b"
               end
        ctx.use(:table_alias)
        ctx.use(:qualified_column)
        where = Expr.predicate(ctx, rng.rand(1..2))
        proj, sortmode, tail = join_tail(ctx)
        query = "SELECT #{proj} FROM #{from} WHERE #{where}#{tail}"

        caps = caps_a | caps_b | Set[SpecData.cap(:insert), SpecData.cap(:insert_multi_row),
                                     SpecData.cap(:select)] | ctx.caps
        Case.new(seed: seed, shape: "join", setup: [ddl_a, ddl_b, insert_a, insert_b],
                 query: query, sortmode: sortmode, caps: caps)
      end

      # tb's base rows plus the join ref `r`: ~60% a hit in 1..na, ~20% a miss, ~20% NULL.
      def b_rows(rng, tb_base, nb, na)
        Data.rows(rng, tb_base, nb).map do |row|
          r = case rng.rand(10)
              when 0..5 then rng.rand(1..na).to_s
              when 6..7 then (na + rng.rand(1..5)).to_s
              else "NULL"
              end
          row + [r]
        end
      end

      def insert(table, rows)
        "INSERT INTO #{table.name} VALUES #{rows.map { |r| "(#{r.join(', ')})" }.join(', ')}"
      end

      # [projection, sortmode, tail]. nosort orders by (a.id, b.id) — unique per output row.
      def join_tail(ctx)
        proj = if ctx.chance(0.4)
                 ctx.use(:select_star)
                 "*"
               else
                 subset(ctx, ctx.columns).map(&:ref).join(", ")
               end
        return [proj, "rowsort", ""] unless ctx.chance(0.65)

        ctx.use(:order_by)
        keys = order_keys(ctx)
        tail = " ORDER BY #{keys.join(', ')}"
        if ctx.chance(0.4)
          ctx.use(:limit)
          tail += " LIMIT #{ctx.rand(0..10)}"
          (ctx.use(:offset); tail += " OFFSET #{ctx.rand(0..4)}") if ctx.chance(0.5)
        end
        [proj, "nosort", tail]
      end

      # 0-2 random extra sort keys (qualified, text collated, optional DESC/NULLS) then a.id, b.id.
      def order_keys(ctx)
        non_pk = ctx.columns.reject { |c| c.ref.end_with?(".id") }
        keys = subset(ctx, non_pk, allow_empty: true, max: 2).map do |c|
          ctx.use(:order_by_keys)
          key = c.ref
          if c.family == :text
            ctx.use(:collate)
            key += ' COLLATE "C"'
          end
          key += " DESC" if ctx.chance(0.5)
          key += ctx.chance(0.5) ? " NULLS FIRST" : " NULLS LAST" if ctx.chance(0.4)
          key
        end
        keys + ["a.id", "b.id"]
      end

      def subset(ctx, cols, allow_empty: false, max: nil)
        chosen = cols.select { ctx.chance(0.5) }
        chosen = [ctx.pick(cols)] if chosen.empty? && !allow_empty && !cols.empty?
        max ? chosen.first(max) : chosen
      end
    end

    REGISTRY["join"] = SelectJoin.method(:generate)
  end
end
