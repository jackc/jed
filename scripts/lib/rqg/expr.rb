# frozen_string_literal: true

require_relative "gen"

module RQG
  # The type-aware boolean-predicate generator — the heart of the firehose. Every node is well-typed
  # by construction (operands of a comparison share a comparable family) so a generated WHERE clause
  # is one PG and jed BOTH accept and agree on. All text participating in any comparison / ordering
  # context routes through the single COLLATE "C" chokepoint (`tc`), because jed's default collation
  # resolves to C while PG's is en_US (testing-ideas.md §4) — forcing C makes PG agree, no false
  # divergence, no ledger entry.
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
      "#{operand(ctx, col.ref, :text == fam)} IS #{neg}DISTINCT FROM #{rhs(ctx, col, fam)}"
    end

    def cmp(ctx, col, fam)
      op = comparison_op(ctx, fam)
      "#{operand(ctx, col.ref, :text == fam)} #{op} #{rhs(ctx, col, fam)}"
    end

    # NOTE on collation: COLLATE "C" is applied ONLY to the LHS column operand (always an a_expr
    # position) — an explicit collation on one operand governs the whole comparison in PostgreSQL,
    # so this both forces C (agreeing with jed's default) AND stays legal where a literal collation
    # would not: BETWEEN's bounds are b_expr, which has no COLLATE production (a PG syntax error).

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
      # bounds are bare literals (b_expr — no COLLATE); the LHS's explicit C governs the comparison.
      "#{operand(ctx, col.ref, :text == fam)} #{neg}BETWEEN #{lo} AND #{hi}"
    end

    def in_list(ctx, col, fam)
      ctx.use(:in_list)
      vals = Array.new(ctx.rand(1..3)) { literal(ctx, fam) }
      neg = ctx.chance(0.25) ? "NOT " : ""
      "#{operand(ctx, col.ref, :text == fam)} #{neg}IN (#{vals.join(', ')})"
    end

    def like(ctx, col)
      kw = ctx.chance(0.5) ? "LIKE" : "ILIKE"
      ctx.use(kw == "LIKE" ? :like : :ilike)
      neg = ctx.chance(0.25) ? "NOT " : ""
      "#{operand(ctx, col.ref, true)} #{neg}#{kw} #{RQG::Data.quote(ctx.pick(LIKE_PATTERNS))}"
    end

    # --- operands / literals ---------------------------------------------------------------------

    # The right-hand operand of a comparison: a bare literal, or sometimes a bare comparable column
    # (never collated — the LHS's explicit COLLATE "C" already governs the comparison).
    def rhs(ctx, col, fam)
      others = ctx.columns.reject { |c| c.ref == col.ref }
                  .select { |c| RQG.comparable?(c.family, fam) }
      if !others.empty? && ctx.chance(0.30)
        ctx.pick(others).ref
      else
        literal(ctx, fam)
      end
    end

    # The COLLATE "C" chokepoint — applied ONLY to a text LHS column operand (always an a_expr).
    def operand(ctx, sql, is_text)
      return sql unless is_text

      ctx.use(:collate)
      "#{sql} COLLATE \"C\""
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
