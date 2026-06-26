# frozen_string_literal: true

require "set"
require_relative "spec_data"

# RQG generator core — the type model, the random schema/data generators, and the generation
# context. Types flow bottom-up so every expression the generator builds is well-typed by
# construction (the subset-staying invariant — scripts/lib/rqg/expr.rb is the heart that relies on
# it). Everything is driven by an injected Random so a case is fully reproducible from its seed.
module RQG
  Column = Struct.new(:name, :type, :nullable, :pk)

  Table = Struct.new(:name, :columns) do
    def pk = columns.find(&:pk)
    def by_family(fam) = columns.select { |c| RQG.family(c.type) == fam }
    def families = columns.map { |c| RQG.family(c.type) }.uniq
  end

  module_function

  # jed type name -> generator family symbol.
  def family(type_name)
    return :integer if SpecData::INT_TYPES.include?(type_name)

    { "boolean" => :boolean, "text" => :text, "decimal" => :decimal }[type_name]
  end

  # Two scalar families that compare cleanly in BOTH PG and jed (so a generated comparison never
  # manufactures a false divergence): same family, or integer<->decimal (both promote to decimal).
  def comparable?(fa, fb)
    return true if fa == fb

    [fa, fb].sort == %i[decimal integer]
  end

  # An in-scope column reference for the expression generator: its SQL text (bare `a` for a single
  # table, qualified `t1.a` in a join), family, jed type, and nullability. Decoupling Expr from a
  # concrete Table is what lets joins/subqueries reuse the same well-typed predicate generator.
  ColRef = Struct.new(:ref, :family, :type, :nullable)

  # Map a table's columns to ColRefs, optionally qualified by an alias/name (for joins).
  def col_refs(table, qual = nil)
    table.columns.map { |c| ColRef.new(qual ? "#{qual}.#{c.name}" : c.name, family(c.type), c.type, c.nullable) }
  end

  # The generation context threaded through expr/shape builders: the PRNG, the in-scope column refs,
  # the capabilities used so far (-> the `# requires:` header), and a recursion-depth budget guarding
  # the parser's MAX_EXPR_DEPTH / 54001 gate (CLAUDE.md §13).
  class Ctx
    attr_reader :rng, :columns, :caps

    def initialize(rng, columns)
      @rng = rng
      @columns = columns
      @caps = Set.new
    end

    def use(feature) = @caps << SpecData.cap(feature)
    def use_type(type_name) = @caps << SpecData.type_cap(type_name)
    def pick(arr) = arr[@rng.rand(arr.size)]
    def chance(prob) = @rng.rand < prob
    def rand(arg) = @rng.rand(arg)
  end

  # Random schema: an integer single-column PRIMARY KEY plus 2-4 scalar columns from the V1 set.
  module Schema
    NAMES = %w[a b c d e f g].freeze
    PK_TYPES = %w[i32 i32 i32 i16 i64].freeze         # mostly i32
    COL_TYPES = %w[i16 i32 i64 boolean text decimal].freeze

    module_function

    def gen(rng, name)
      cols = [Column.new("id", PK_TYPES[rng.rand(PK_TYPES.size)], false, true)]
      names = NAMES.dup
      ncol = rng.rand(2..4)
      ncol.times do
        t = COL_TYPES[rng.rand(COL_TYPES.size)]
        nullable = rng.rand >= 0.30 # ~30% of non-pk columns are NOT NULL
        cols << Column.new(names.shift, t, nullable, false)
      end
      Table.new(name, cols)
    end

    # The CREATE TABLE statement + the set of features it uses.
    def ddl(table)
      caps = Set[SpecData.cap(:create_table), SpecData.cap(:primary_key)]
      defs = table.columns.map do |c|
        caps << SpecData.type_cap(c.type)
        s = "#{c.name} #{c.type}"
        if c.pk
          s += " PRIMARY KEY"
        elsif !c.nullable
          s += " NOT NULL"
          caps << SpecData.cap(:not_null)
        end
        s
      end
      ["CREATE TABLE #{table.name} (#{defs.join(', ')})", caps]
    end
  end

  # Random data: unique integer PKs, ~20% NULLs in nullable columns, values biased to small
  # magnitudes plus the type boundary set. Literals render identically for PG and jed (byte-identical
  # data on both sides).
  module Data
    # Pure-ASCII, no empty string and no leading/trailing whitespace: jed's sqllogictest variant
    # cannot represent an empty/whitespace result cell (a blank line ends the record and every
    # expected line is trim()'d — impl/rust/src/bin/conformance.rs), so such a value in a projected
    # text column would manufacture a false divergence. The §4 look-alike hazard never applies (ASCII).
    TEXT_POOL = %w[apple Apple banana BANANA cat Cat dog x AB ab z mango Mango ZZ].freeze

    module_function

    def rows(rng, table, n)
      (1..n).map do |i|
        table.columns.map do |c|
          if c.pk
            i.to_s
          elsif c.nullable && rng.rand < 0.20
            "NULL"
          else
            literal(rng, c.type)
          end
        end
      end
    end

    def literal(rng, type)
      case RQG.family(type)
      when :integer then int_literal(rng, type)
      when :boolean then rng.rand < 0.5 ? "TRUE" : "FALSE"
      when :decimal then decimal_literal(rng)
      when :text then text_literal(rng)
      end
    end

    def int_literal(rng, type)
      r = SpecData.int_ranges[type]
      case rng.rand(10)
      when 0..5 then rng.rand(-10..10).to_s
      when 6..7 then rng.rand(-1000..1000).to_s
      else [r[:min], r[:min] + 1, 0, r[:max] - 1, r[:max]][rng.rand(5)].to_s
      end
    end

    def decimal_literal(rng)
      whole = rng.rand(-1000..1000)
      return whole.to_s if rng.rand < 0.4

      "#{whole}.#{format('%02d', rng.rand(0..99))}"
    end

    def text_literal(rng)
      quote(TEXT_POOL[rng.rand(TEXT_POOL.size)])
    end

    # Single-quote a text literal, doubling embedded quotes (the pool is pure ASCII — the §4
    # look-alike hazard never applies).
    def quote(str) = "'#{str.gsub("'", "''")}'"
  end
end
