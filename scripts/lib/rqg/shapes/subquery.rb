# frozen_string_literal: true

require_relative "../gen"
require_relative "../expr"
require_relative "../case"

module RQG
  module Shapes
    # subquery — a main table filtered by an UNCORRELATED subquery over a second table: IN / NOT IN
    # (integer membership), [NOT] EXISTS, or a scalar comparison against an AGGREGATE subquery (always
    # exactly one row, so no 21000 cardinality risk). The subquery carries its own WHERE over the
    # second table. Membership/existence use equality + 3VL, which agree in PG and jed; the only
    # collation-sensitive spot is a text MIN/MAX in a scalar subquery, routed through COLLATE "C".
    module Subquery
      module_function

      def generate(seed)
        rng = Random.new(seed)
        main = Schema.gen(rng, "m#{seed}")
        sub = Schema.gen(rng, "s#{seed}")
        ddl_m, caps_m = Schema.ddl(main)
        ddl_s, caps_s = Schema.ddl(sub)
        insert_m = insert(main, Data.rows(rng, main, rng.rand(5..9)))
        insert_s = insert(sub, Data.rows(rng, sub, rng.rand(4..8)))

        outer = Ctx.new(rng, RQG.col_refs(main))
        inner = Ctx.new(rng, RQG.col_refs(sub))
        pred = subquery_pred(outer, inner, main, sub)
        # optionally AND a plain predicate over the main table
        where = outer.chance(0.4) ? "(#{pred}) AND (#{Expr.predicate(outer, 1)})" : pred

        proj, sortmode, tail = main_tail(outer, main)
        query = "SELECT #{proj} FROM #{main.name} WHERE #{where}#{tail}"
        caps = caps_m | caps_s | Set[SpecData.cap(:insert), SpecData.cap(:insert_multi_row),
                                     SpecData.cap(:select)] | outer.caps | inner.caps
        Case.new(seed: seed, shape: "subquery", setup: [ddl_m, ddl_s, insert_m, insert_s],
                 query: query, sortmode: sortmode, caps: caps)
      end

      def subquery_pred(outer, inner, main, sub)
        sub_where = Expr.predicate(inner, 1)
        kind = outer.pick(%i[in exists scalar])
        case kind
        when :exists
          outer.use(:subquery_exists)
          neg = outer.chance(0.4) ? "NOT " : ""
          "#{neg}EXISTS (SELECT 1 FROM #{sub.name} WHERE #{sub_where})"
        when :in
          outer.use(:subquery_in)
          mcol = outer.pick(int_cols(main))
          scol = outer.pick(int_cols(sub))
          neg = outer.chance(0.35) ? "NOT " : ""
          "#{mcol.name} #{neg}IN (SELECT #{scol.name} FROM #{sub.name} WHERE #{sub_where})"
        else
          outer.use(:subquery_scalar)
          scalar_pred(outer, inner, main, sub, sub_where)
        end
      end

      # `mcol <op> (SELECT <agg> FROM sub WHERE ...)` — agg guarantees one row.
      def scalar_pred(outer, inner, main, sub, sub_where)
        if outer.chance(0.5)
          mcol = outer.pick(int_cols(main))
          inner.use(:aggregates)
          "#{mcol.name} #{outer.pick(%w[= <> < > <= >=])} " \
            "(SELECT count(*) FROM #{sub.name} WHERE #{sub_where})"
        else
          scol = outer.pick(int_cols(sub))
          mcol = outer.pick(int_cols(main))
          inner.use(:aggregates)
          fn = outer.pick(%w[min max sum])
          "#{mcol.name} #{outer.pick(%w[= <> < > <= >=])} " \
            "(SELECT #{fn}(#{scol.name}) FROM #{sub.name} WHERE #{sub_where})"
        end
      end

      def int_cols(table)
        cols = table.columns.select { |c| RQG.family(c.type) == :integer }
        cols.empty? ? [table.pk] : cols
      end

      def main_tail(ctx, main)
        proj = if ctx.chance(0.5)
                 ctx.use(:select_star)
                 "*"
               else
                 cols = main.columns.select { ctx.chance(0.6) }
                 cols = [main.pk] if cols.empty?
                 cols.map(&:name).join(", ")
               end
        if ctx.chance(0.7)
          ctx.use(:order_by)
          [proj, "nosort", " ORDER BY #{main.pk.name}"]
        else
          [proj, "rowsort", ""]
        end
      end

      def insert(table, rows)
        "INSERT INTO #{table.name} VALUES #{rows.map { |r| "(#{r.join(', ')})" }.join(', ')}"
      end
    end

    REGISTRY["subquery"] = Subquery.method(:generate)
  end
end
