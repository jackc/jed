# frozen_string_literal: true

require_relative "../gen"
require_relative "../expr"
require_relative "../case"

module RQG
  module Shapes
    # group_by — GROUP BY one or two bare columns, a WHERE filter, 1-3 aggregates (COUNT(*),
    # COUNT(col), SUM/AVG over numeric, MIN/MAX over int/decimal/text), an optional HAVING over an
    # aggregate, and either ORDER BY the grouping columns (unique per group → nosort) or rowsort.
    #
    # Subset-staying: jed's aggregate type rules match PG (sum(i16/i32)→i64, sum(i64)→decimal,
    # avg→decimal, count→i64) so no false divergence. MIN/MAX is restricted to int/decimal/text
    # (PG ships no min/max for boolean/uuid — a jed extension that would skip), and MIN/MAX(text)
    # routes through COLLATE "C" (collation-ordered). GROUP BY uses equality (deterministic
    # collation = byte-equal), so it agrees C-vs-en_US without COLLATE; only ORDER BY of a text
    # group key needs it.
    module GroupBy
      module_function

      def generate(seed)
        rng = Random.new(seed)
        table = Schema.gen(rng, "g#{seed}")
        ddl, ddl_caps = Schema.ddl(table)
        nrows = rng.rand(6..12)
        insert = "INSERT INTO #{table.name} VALUES " \
                 "#{Data.rows(rng, table, nrows).map { |r| "(#{r.join(', ')})" }.join(', ')}"

        ctx = Ctx.new(rng, RQG.col_refs(table))
        where = Expr.predicate(ctx, rng.rand(1..2))
        group_cols = subset(ctx, table.columns, max: 2)
        aggs = aggregates(ctx, table)
        select_items = group_cols.map(&:name) + aggs
        having = ctx.chance(0.45) ? " HAVING #{having_pred(ctx, table)}" : ""
        order, sortmode = group_order(ctx, group_cols)

        query = "SELECT #{select_items.join(', ')} FROM #{table.name} WHERE #{where} " \
                "GROUP BY #{group_cols.map(&:name).join(', ')}#{having}#{order}"
        caps = ddl_caps | Set[SpecData.cap(:insert), SpecData.cap(:insert_multi_row),
                              SpecData.cap(:select), SpecData.cap(:group_by)] | ctx.caps
        Case.new(seed: seed, shape: "group_by", setup: [ddl, insert],
                 query: query, sortmode: sortmode, caps: caps)
      end

      # 1-3 aggregate terms.
      def aggregates(ctx, table)
        num = table.columns.select { |c| %i[integer decimal].include?(RQG.family(c.type)) }
        ord = table.columns.select { |c| %i[integer decimal text].include?(RQG.family(c.type)) }
        choices = [-> { "count(*)" }, -> { "count(#{ctx.pick(table.columns).name})" }]
        unless num.empty?
          choices << -> { "sum(#{ctx.pick(num).name})" }
          choices << -> { "avg(#{ctx.pick(num).name})" }
        end
        choices << -> { minmax(ctx, ctx.pick(ord)) } unless ord.empty?
        ctx.use(:aggregates)
        Array.new(ctx.rand(1..3)) { ctx.pick(choices).call }
      end

      def minmax(ctx, col)
        fn = ctx.chance(0.5) ? "min" : "max"
        arg = col.name
        if RQG.family(col.type) == :text
          ctx.use(:collate)
          arg += ' COLLATE "C"'
        end
        "#{fn}(#{arg})"
      end

      # A HAVING predicate over an aggregate — count-based, plus sum-based when a numeric column exists.
      def having_pred(ctx, table)
        ctx.use(:having)
        num = table.columns.select { |c| %i[integer decimal].include?(RQG.family(c.type)) }
        op = ctx.pick(%w[> >= < <= <> =])
        if num.empty? || ctx.chance(0.5)
          "count(*) #{op} #{ctx.rand(0..4)}"
        else
          "sum(#{ctx.pick(num).name}) #{op} #{ctx.rng.rand(-50..50)}"
        end
      end

      # ORDER BY the grouping columns (unique per group → total order, nosort), or rowsort.
      def group_order(ctx, group_cols)
        return ["", "rowsort"] unless ctx.chance(0.6)

        ctx.use(:order_by)
        ctx.use(:order_by_keys) if group_cols.size > 1
        keys = group_cols.map do |c|
          if RQG.family(c.type) == :text
            ctx.use(:collate)
            "#{c.name} COLLATE \"C\""
          else
            c.name
          end
        end
        [" ORDER BY #{keys.join(', ')}", "nosort"]
      end

      def subset(ctx, cols, max:)
        chosen = cols.select { ctx.chance(0.5) }
        chosen = [ctx.pick(cols)] if chosen.empty?
        chosen.first(max)
      end
    end

    REGISTRY["group_by"] = GroupBy.method(:generate)
  end
end
