# frozen_string_literal: true

require "toml-rb"
require "set"

module RQG
  # SpecData — the canonical shared spec tables the generator samples from (CLAUDE.md §5: generate
  # from data, not hardcoded SQL). Loads the scalar type ranges from spec/types/scalars.toml and the
  # capability registry from spec/conformance/manifest.toml, and validates every capability the
  # generator might attach to a `# requires:` header actually EXISTS (so `rake verify` never sees an
  # orphan cap). The V1 scalar set + per-feature capability map are curated here; the *ranges* come
  # from scalars.toml so they stay in sync with the engine.
  module SpecData
    ROOT = File.expand_path("../../..", __dir__)
    SCALARS = File.join(ROOT, "spec/types/scalars.toml")
    MANIFEST = File.join(ROOT, "spec/conformance/manifest.toml")

    # The V1 generable scalar families. Integer ranges are read from scalars.toml below.
    FAMILIES = %i[integer boolean text decimal].freeze
    INT_TYPES = %w[i16 i32 i64].freeze

    # feature symbol -> the manifest capability id it requires. Every value is checked to exist.
    CAPS = {
      create_table: "ddl.create_table",
      primary_key: "ddl.primary_key",
      not_null: "ddl.not_null",
      insert: "dml.insert",
      insert_multi_row: "dml.insert_multi_row",
      select: "query.select",
      select_star: "query.select_star",
      column_alias: "query.column_alias",
      where_eq: "query.where_eq",
      comparison_order: "query.comparison_order",
      not_equal: "expr.not_equal",
      is_null: "query.is_null",
      is_distinct_from: "query.is_distinct_from",
      between: "expr.between",
      in_list: "expr.in_list",
      like: "expr.like",
      ilike: "expr.ilike",
      collate: "expr.collate",
      parens: "expr.parens",
      order_by: "query.order_by",
      order_by_keys: "query.order_by_keys",
      limit: "query.limit",
      offset: "query.offset",
      distinct: "query.distinct",
      type_i16: "types.i16",
      type_i32: "types.i32",
      type_i64: "types.i64",
      type_boolean: "types.boolean",
      type_text: "types.text",
      type_decimal: "types.decimal",
    }.freeze

    class << self
      # { "i16" => {min:, max:}, ... } from scalars.toml (the source of truth for ranges).
      def int_ranges
        @int_ranges ||= begin
          doc = TomlRB.load_file(SCALARS)
          types = doc["type"] || []
          INT_TYPES.each_with_object({}) do |name, h|
            t = types.find { |x| x["id"] == name } or raise "scalars.toml missing #{name}"
            h[name] = { min: t["min"], max: t["max"] }
          end
        end
      end

      def all_caps
        @all_caps ||= begin
          doc = TomlRB.load_file(MANIFEST)
          (doc["capability"] || []).map { |c| c["id"] }.compact.to_set
        end
      end

      # The manifest id for a feature symbol; raises if the curated CAPS map names a non-existent
      # capability (fail fast — keeps emitted `# requires:` headers verify-clean).
      def cap(feature)
        id = CAPS.fetch(feature) { raise "unknown RQG feature #{feature.inspect}" }
        raise "RQG cap #{id.inspect} (#{feature}) not in manifest.toml" unless all_caps.include?(id)

        id
      end

      def type_cap(type_name)
        cap(:"type_#{type_name}")
      end

      # Validate the whole curated CAPS map against the manifest up front.
      def validate!
        CAPS.each_key { |f| cap(f) }
        int_ranges
        true
      end
    end
  end
end
