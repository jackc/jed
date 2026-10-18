# frozen_string_literal: true

module JedNative
  # Pin changes belong to the same working snapshot as the keys they rebuild.
  # This is a host operation; no SQL function can invoke it.
  module CollationUpgrade
    def snapshot
      super + [{ native_collation_snapshot: true, pins: @storage_collations, default: @default_collation }]
    end

    def restore(state)
      if state.last.is_a?(Hash) && state.last[:native_collation_snapshot]
        pins = state.last
        @storage_collations, @default_collation = pins.values_at(:pins, :default)
        super(state[0...-1])
      else
        super
      end
    end

    def upgrade_collations
      raise Error.new('55000', 'database is closed') if @closed
      raise Error.new('25P02', 'transaction is aborted') if @failed
      raise Error.new('25006', 'cannot upgrade collations in a read-only database or transaction') if @read_only || @storage&.read_only?
      references = @storage_collations || []
      changed = references.filter_map do |entry|
        loaded = resolve_collation(entry[:name])
        entry[:name] if [entry[:unicode], entry[:cldr]] != [loaded.unicode_version, loaded.cldr_version]
      end
      return 0 if changed.empty?

      before, dirty = snapshot, @dirty
      old_cost, old_lifetime = @cost, @lifetime_cost
      old_max, old_lifetime_max = @max_cost, @lifetime_max_cost
      begin
        @max_cost = @lifetime_max_cost = 0
        @tables = @tables.transform_values do |table|
          next table if table[:database] || (table[:collation_skew].to_a & changed).empty?
          clean = table.reject { |key, _| key.to_s.start_with?('native_') || %i[storage_tree_profile storage_tree_rows].include?(key) }
          clean.merge(collation_skew: table[:collation_skew] - changed,
                      indexes: table[:indexes].map { |index| index.reject { |key, _| key == :storage_tree_profile } })
        end
        refresh_primary_indexes
        @storage_collations = references.map do |entry|
          next entry unless changed.include?(entry[:name])
          loaded = resolve_collation(entry[:name])
          entry.merge(unicode: loaded.unicode_version, cldr: loaded.cldr_version, desc: loaded.description)
        end
        @native_select_plans&.clear
        @dirty = true
        persist unless @transaction
        changed.length
      rescue StandardError
        restore(before)
        @dirty = dirty
        raise
      ensure
        @cost, @lifetime_cost = old_cost, old_lifetime
        @max_cost, @lifetime_max_cost = old_max, old_lifetime_max
      end
    end
  end
  Engine.prepend(CollationUpgrade)
end
