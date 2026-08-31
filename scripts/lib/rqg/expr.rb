# frozen_string_literal: true

require_relative "gen"

module RQG
  # The type-aware boolean-predicate generator — the heart of the firehose. Every node is well-typed
  # by construction (operands of a comparison share a comparable family) so a generated WHERE clause
  # is one PG and jed BOTH accept and agree on.
  #
  # Text needs no special handling. It used to: every text operand was routed through a
  # COLLATE "C" chokepoint because the oracle database was initdb'd under the libc en_US locale
  # collation while jed's default is `C`. That was scaffolding for the SERVER's configuration, not
  # for a semantic difference — and it cost real coverage (see shapes/setop.rb). The oracle now runs
  # on the platform-independent builtin C.UTF-8 provider, whose ordering IS code-point order
  # (spec/conformance/oracle_profile.toml), so a bare text comparison agrees with jed by
  # construction and the generator emits the SQL a user would actually write.
  module Expr
    module_function

    LIKE_PATTERNS = ["a%", "%a", "%a%", "_a%", "A%", "%", "ab%", "%z", "_", "cat", "%n_"].freeze

    # A boolean predicate tree of at most `depth` AND/OR/NOT nesting.
    def predicate(ctx, depth)
      return leaf(ctx) if depth <= 0 || ctx.chance(0.45)

      case ctx.rand(3)
      when 0
        op = ctx.pick(%w[AND OR])
        ctx.use(:parens)
        "(#{predicate(ctx, depth - 1)} #{op} #{predicate(ctx, depth - 1)})"
      when 1
        ctx.use(:parens)
        "NOT (#{predicate(ctx, depth - 1)})"
      else
        leaf(ctx)
      end
    end

    # A single comparison/test predicate over one in-scope column, matched to the column's family.
    def leaf(ctx)
      col = ctx.pick(ctx.columns)
      fam = col.family
      case ctx.pick(kinds(fam))
      when :is_null then is_null(ctx, col)
      when :idf then idf(ctx, col, fam)
      when :cmp then cmp(ctx, col, fam)
      when :between then between(ctx, col, fam)
      when :in_list then in_list(ctx, col, fam)
      when :like then like(ctx, col)
      when :bool_col then ctx.chance(0.5) ? col.ref : "NOT #{col.ref}"
      end
    end

    def kinds(fam)
      case fam
      when :boolean then %i[is_null idf cmp bool_col]
      when :text then %i[is_null idf cmp between in_list like]
      else %i[is_null idf cmp between in_list] # integer / decimal
      end
    end

    # --- predicate forms -------------------------------------------------------------------------

    def is_null(ctx, col)
      ctx.use(:is_null)
      "#{col.ref} IS #{ctx.chance(0.5) ? 'NOT ' : ''}NULL"
    end

    def idf(ctx, col, fam)
      ctx.use(:is_distinct_from)
      neg = ctx.chance(0.5) ? "NOT " : ""
      "#{col.ref} IS #{neg}DISTINCT FROM #{rhs(ctx, col, fam)}"
    end

    def cmp(ctx, col, fam)
      op = comparison_op(ctx, fam)
      "#{col.ref} #{op} #{rhs(ctx, col, fam)}"
    end

    def comparison_op(ctx, fam)
      ops = fam == :boolean ? %w[= <>] : %w[= <> < > <= >=]
      op = ctx.pick(ops)
      case op
      when "=" then ctx.use(:where_eq)
      when "<>" then ctx.use(:not_equal)
      else ctx.use(:comparison_order)
      end
      op
    end

    def between(ctx, col, fam)
      ctx.use(:between)
      lo, hi = order_pair(literal(ctx, fam), literal(ctx, fam), fam)
      neg = ctx.chance(0.25) ? "NOT " : ""
      "#{col.ref} #{neg}BETWEEN #{lo} AND #{hi}"
    end

    def in_list(ctx, col, fam)
      ctx.use(:in_list)
      vals = Array.new(ctx.rand(1..3)) { literal(ctx, fam) }
      neg = ctx.chance(0.25) ? "NOT " : ""
      "#{col.ref} #{neg}IN (#{vals.join(', ')})"
    end

    def like(ctx, col)
      kw = ctx.chance(0.5) ? "LIKE" : "ILIKE"
      ctx.use(kw == "LIKE" ? :like : :ilike)
      neg = ctx.chance(0.25) ? "NOT " : ""
      "#{col.ref} #{neg}#{kw} #{RQG::Data.quote(ctx.pick(LIKE_PATTERNS))}"
    end

    # --- operands / literals ---------------------------------------------------------------------

    # The right-hand operand of a comparison: a bare literal, or sometimes a bare comparable column.
    def rhs(ctx, col, fam)
      others = ctx.columns.reject { |c| c.ref == col.ref }
                  .select { |c| RQG.comparable?(c.family, fam) }
      if !others.empty? && ctx.chance(0.30)
        ctx.pick(others).ref
      else
        literal(ctx, fam)
      end
    end

    # A bare literal value of the given family.
    def literal(ctx, fam)
      case fam
      when :integer then ctx.rng.rand(-1000..1000).to_s
      when :decimal then RQG::Data.decimal_literal(ctx.rng)
      when :boolean then ctx.chance(0.5) ? "TRUE" : "FALSE"
      when :text then RQG::Data.text_literal(ctx.rng)
      end
    end

    # Order a literal pair low..high for BETWEEN (integers/decimals compare numerically; text/boolean
    # are left as-is — BETWEEN agrees either way, an out-of-order range just yields no rows).
    def order_pair(a, b, fam)
      return [a, b].sort_by(&:to_f) if %i[integer decimal].include?(fam)

      [a, b]
    end
  end
end
