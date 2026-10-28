# frozen_string_literal: true

require 'set'

module JedNative
  # The integrity check (spec/design/integrity-check.md): a host-API walk of one database's pinned
  # generation that reports structured findings instead of failing on the first problem. It reads the
  # catalog chain, every table and index tree, overflow chains and the free list, and checks the
  # invariants between them; `full` probes each index for the entries its table derives, and
  # `constraints` re-checks the declared constraints against the stored rows.
  #
  # The report is the cross-core contract of §4: the same findings (all but `detail`) in the same
  # order, the same skips and counters as the Rust/Go/TS cores and spec/fileformat/integrity.rb.
  #
  # Divergence from §5.4, recorded for this experimental core: a table's decoded rows and an index's
  # entries are held in memory for the agreement and constraint passes, so the check's memory grows
  # with the largest table instead of being bounded by one root-to-leaf path. It is unmetered, like
  # compaction, and charges no session account.
  IntegrityFinding = Struct.new(:kind, :database, :object, :page, :key, :detail, keyword_init: true)
  IntegritySkip = Struct.new(:object, :reason, keyword_init: true)
  IntegrityReport = Struct.new(:findings, :truncated, :skipped, :pages, :rows, keyword_init: true) do
    def ok? = findings.empty? && !truncated
  end

  # The registered finding kinds and skip reasons (spec/integrity/findings.toml), cross-checked by a
  # unit test.
  INTEGRITY_KINDS = %w[
    meta_invalid manifest_page_mismatch catalog_page_corrupt catalog_mismatch sequence_out_of_range
    page_out_of_range page_reached_twice page_corrupt page_type_wrong tree_depth_uneven keys_unordered
    separator_violation row_key_not_primary_key value_undecodable overflow_chain_corrupt
    row_count_mismatch gist_bounds_violation
    live_count_mismatch free_list_corrupt free_page_out_of_range free_page_duplicate free_page_live
    free_page_manifest_dependency
    index_entry_missing index_entry_count_mismatch index_entry_error
    not_null_violation typmod_violation check_violation check_error unique_violation foreign_key_violation
  ].freeze
  INTEGRITY_SKIP_REASONS = %w[
    unparsable_definition missing_host_function missing_loaded_data table_unreadable index_unreadable
  ].freeze

  # Reads whole pages of an in-memory domain as an image (pages it never wrote read as zeros).
  class IntegrityPagesImage
    def initialize(pages, page_size, page_count)
      @pages, @page_size, @page_count = pages, page_size, page_count
    end

    def bytesize = @page_size * @page_count

    def byteslice(offset, length)
      out = +''.b
      while length.positive?
        page, within = offset.divmod(@page_size)
        bytes = @pages[page] || ("\x00".b * @page_size)
        piece = bytes.byteslice(within, [length, @page_size - within].min)
        out << piece
        offset += piece.bytesize
        length -= piece.bytesize
      end
      out
    end
  end

  class IntegrityCheck
    MAX_TREE_DEPTH = 256

    class Truncated < StandardError; end

    class Reach < StandardError
      attr_reader :kind, :page

      def initialize(kind, page)
        @kind, @page = kind, page
        super("page #{page} #{kind == 'page_reached_twice' ? 'reached twice' : 'out of range'}")
      end
    end

    # image: whole-page reads of the pinned generation; meta: its root/page_count/free_head/live and
    # manifest; durable: whether the meta, manifest and free list live in the file (an in-memory domain
    # keeps them in RAM: memory_free is its free list). engine: the pinned session's engine, which
    # evaluates expression keys, predicates and CHECKs.
    def initialize(database:, image:, page_size:, meta:, engine:, catalog_engine:, durable:, memory_free: nil,
                   level: :full, constraints: false, max_findings: 100, crypto: nil)
      level = level.to_s
      raise Error.new('22023', "unknown integrity check level #{level}") unless %w[quick full].include?(level)
      raise Error.new('22023', 'max_findings must be a nonnegative integer') unless max_findings.is_a?(Integer) && max_findings >= 0

      @database, @image, @ps, @meta, @engine = database, image, page_size, meta, engine
      @catalog_engine = catalog_engine
      @durable, @memory_free = durable, memory_free
      @level, @constraints, @max = level, constraints, max_findings
      @codec = FileFormat.new(collation_key: lambda { |name, value|
        collation = engine.resolve_collation(name)
        collation ? collation.sort_key(value) : value.b
      }, crypto: crypto)
      @findings, @skipped = [], []
      @truncated = false
      @reached = Set.new
      @rows = 0
      @damaged = false
      @free_chain_pages = 0
      @tables = {}
      @index_clean = {}
      @index_entries = {}
      @first = @codec.first_body
    end

    def run
      begin
        check_generation
        catalog = load_catalog
        if catalog
          @collation_entries = catalog[:collations]
          check_sequences(catalog[:sequences])
          tables = catalog[:tables].sort_by { |t| t[:name].b }
          @by_name = tables.to_h { |t| [t[:name], t] }
          tables.each { |t| check_table(t) }
          tables.each { |t| check_constraints(t) } if @constraints
        end
        check_free_list
        if @durable && !@damaged && @meta[:live] != @reached.size
          add('live_count_mismatch', detail: "meta records #{@meta[:live]} live pages, #{@reached.size} are reachable")
        end
      rescue Truncated
        @truncated = true
      end
      IntegrityReport.new(findings: @findings, truncated: @truncated, skipped: @skipped,
                          pages: @reached.size + @free_chain_pages, rows: @rows)
    end

    private

    def add(kind, object: nil, page: nil, key: nil, detail: '')
      raise Truncated if !@max.zero? && @findings.size == @max

      @findings << IntegrityFinding.new(kind: kind, database: @database, object: object, page: page,
                                        key: key&.b, detail: detail)
    end

    def skip(object, reason) = @skipped << IntegritySkip.new(object: object, reason: reason)

    def read_page(page) = @codec.read_page(@image, @ps, page)

    # --- generation (§3.1) ----------------------------------------------------------------------------

    # A live handle's meta slot and commit manifest are re-verified: damage since open is reported
    # (meta_invalid, manifest_page_mismatch) rather than failing the call.
    def check_generation
      return unless @durable

      slot = @meta[:slot]
      reread = slot && @codec.read_meta(@image, @ps, slot, manifest: false)
      if slot && (reread.nil? || reread[:txid] != @meta[:txid])
        add('meta_invalid', page: slot, detail: "meta slot #{slot} no longer verifies")
      end
      if slot && reread && @codec.encrypted?
        stored = @image.byteslice(slot * @ps, @ps)
        manifest = (@meta[:manifest_pages] || []).map { |page| [page, @image.byteslice(page * @ps, @ps)] }
        add('meta_invalid', page: slot, detail: "meta slot #{slot} fails authentication") unless @codec.crypto.meta_authentic?(slot, stored, manifest)
      end
      (@meta[:manifest_pages] || []).each do |page|
        block = @image.byteslice(page * @ps, @ps)
        next if block.getbyte(0) == 8 && @codec.page_crc(block) == block.byteslice(12, 4).unpack1('N') &&
                block.byteslice(16, 8).unpack1('Q>') == @meta[:txid]

        add('manifest_page_mismatch', page: page, detail: 'manifest page does not verify')
      end
      (@meta[:manifest] || []).each do |index, expected|
        if index < @first || index >= @meta[:page_count]
          add('manifest_page_mismatch', page: index, detail: 'listed page outside the body range')
          next
        end
        block = @image.byteslice(index * @ps, @ps)
        # An encrypted body page is ciphertext: its manifest identity alone checks it.
        next if @codec.crc64(block) == expected && (@codec.encrypted? || (1..7).cover?(block.getbyte(0)) &&
                @codec.page_crc(block) == block.byteslice(12, 4).unpack1('N'))

        add('manifest_page_mismatch', page: index, detail: "page #{index} no longer carries its recorded identity")
      end
    end

    # The stored catalog chain: its pages are reached first. A page that fails to verify or decode is
    # catalog_page_corrupt, and no table can then be walked (the generation is damaged).
    def load_catalog
      types, sequences, collations, tables, statistics, expected = [], [], [], [], [], {}
      ctypes = {}
      @codec.instance_variable_set(:@ctypes, ctypes)
      page = @meta[:root_page]
      while page != 0
        unless page >= @first && page < @meta[:page_count] && !@reached.include?(page)
          @damaged = true
          add('catalog_page_corrupt', page: page, detail: "catalog chain names page #{page}")
          return nil
        end
        @reached << page
        current = page
        begin
          pg = read_page(page)
          raise 'expected a catalog page' unless pg[:type] == FileFormat::PAGE_CATALOG && (pg[:flags] & ~1).zero?

          nxt = pg[:next_page]
          payload = pg[:payload]
          if pg[:flags] & 1 == 1
            payload = payload.dup
            loop do
              raise 'a continued catalog entry ends the chain' if nxt.zero?

              current = nxt
              raise "catalog chain names page #{nxt}" unless nxt >= @first && nxt < @meta[:page_count] && @reached.add?(nxt)

              cp = read_page(nxt)
              payload << cp[:payload]
              nxt = cp[:next_page]
              break if (cp[:flags] & 1).zero?
            end
          end
          pos = 0
          pg[:item_count].times do
            kb, pos = @codec.take(payload, pos, 1)
            case kb.getbyte(0)
            when 0
              entry, pos = @codec.decode_table_entry(payload, pos)
              tables << entry
            when 1
              ct, pos = @codec.decode_composite_type_entry(payload, pos)
              types << ct
              ctypes[ct[:name]] = ct[:fields]
            when 2
              s, pos = @codec.decode_sequence_entry(payload, pos)
              sequences << s
            when 3
              c, pos = @codec.decode_collation_entry(payload, pos)
              collations << c
            when 4 then pos = @codec.decode_statistics_entry(payload, pos, tables, statistics, expected)
            when 5 then _view, pos = @codec.decode_view_entry(payload, pos)
            else raise 'unknown catalog entry kind'
            end
          end
        rescue Error, StandardError => error
          raise unless catalog_error?(error)

          @damaged = true
          add('catalog_page_corrupt', page: current, detail: error.message)
          return nil
        end
        page = nxt
      end
      { types: types, sequences: sequences, collations: collations, tables: tables }
    end

    # A failure decoding stored bytes is a finding. This core's decoders report malformed bytes with
    # assorted StandardErrors (a short read surfaces as a nil-receiver NoMethodError), so every one is a
    # decode failure; the walk's own truncation signal is not.
    def catalog_error?(error) = error.is_a?(StandardError) && !error.is_a?(Truncated)

    def check_sequences(sequences)
      sequences.sort_by { |s| s[:name].b }.each do |s|
        next if s[:last_value].between?(s[:min_value], s[:max_value])

        add('sequence_out_of_range', object: s[:name], detail: "value #{s[:last_value]} outside [#{s[:min_value]}, #{s[:max_value]}]")
      end
    end

    # --- trees and pages (§3.2) -----------------------------------------------------------------------

    # Record a page as reached; false (after reporting) when the walk must not read it.
    def reach(object, page)
      unless page >= @first && page < @meta[:page_count]
        add('page_out_of_range', object: object, page: page, detail: "page #{page} outside [#{@first}, #{@meta[:page_count]})")
        return false
      end
      unless @reached.add?(page)
        add('page_reached_twice', object: object, page: page, detail: "page #{page} is already part of the database")
        return false
      end
      true
    end

    def check_table(t)
      rows = []
      walk = BtreeWalk.new(self, t[:name], t[:columns])
      damaged = !walk.run(t[:root_data_page]) { |page, key, values| rows << check_row(t, page, key, values) }
      damaged ||= rows.any?(&:nil?)
      if !damaged && t[:row_count] != rows.size
        add('row_count_mismatch', object: t[:name], detail: "catalog records #{t[:row_count]} rows, the tree holds #{rows.size}")
      end
      @damaged ||= damaged
      @tables[t[:name]] = { table: t, rows: damaged ? nil : rows }
      t[:indexes].sort_by { |ix| ix[:name].b }.each { |ix| check_index(t, ix, damaged ? nil : rows) }
    end

    def overflow_fetch
      @overflow_fetch ||= lambda do |page|
        raise Reach.new('page_out_of_range', page) unless page >= @first && page < @meta[:page_count]
        raise Reach.new('page_reached_twice', page) unless @reached.add?(page)

        read_page(page)
      end
    end

    # Decode one row; nil when a value does not decode (the table is then unreadable).
    def check_row(t, page, key, values)
      @rows += 1
      row = values.each_with_index.map do |(type, raw), ci|
        next nil if raw.nil?

        column = "column #{t[:columns][ci][:name]}"
        begin
          @codec.decode_value(type, raw, 0, overflow_fetch).first
        rescue Reach => e
          add(e.kind, object: t[:name], page: e.page, key: key, detail: "#{column}: #{e.message}")
          return nil
        rescue Error, StandardError => e
          raise unless catalog_error?(e)

          external = [FileFormat::TAG_EXTERNAL, FileFormat::TAG_EXTERNAL_COMP].include?(raw.getbyte(0))
          add(external ? 'overflow_chain_corrupt' : 'value_undecodable', object: t[:name], page: page, key: key, detail: "#{column}: #{e.message}")
          return nil
        end
      end
      unless t[:pk].empty?
        want = begin
          t[:pk].map { |pi| key_body(t[:columns][pi], row[pi]) }.join.b
        rescue Error, StandardError
          nil
        end
        unless want == key
          add('row_key_not_primary_key', object: t[:name], page: page, key: key, detail: "stored key is not the row's primary-key encoding")
        end
      end
      [key, row]
    end

    def key_body(column, value)
      collation = column[:collation]
      collation = nil if collation == 'C'
      @codec.key_body(column[:type], value, collation)
    end

    # An index whose walk reported anything is not checked for agreement (§5.2).
    def check_index(t, ix, rows)
      before = @findings.size
      entries = []
      if ix[:kind] == 'gist'
        ops = @codec.gist_opclasses_for(t, ix)
        walk = GistWalk.new(self, ix[:name], ops)
        readable = walk.run(ix[:root_page])
        entries = walk.entries
        count = entries.size
      else
        walk = BtreeWalk.new(self, ix[:name], nil, gin: ix[:kind] == 'gin')
        readable = walk.run(ix[:root_page]) { |_page, key, _values| entries << key }
        count = ix[:kind] == 'gin' ? walk.postings : entries.size
      end
      @damaged ||= !readable
      clean = readable && @findings.size == before
      @index_clean[ix[:name]] = clean
      @index_entries[ix[:name]] = entries
      return unless @level == 'full'

      definition = resolve_index(t, ix)
      return skip(ix[:name], definition) if definition.is_a?(String)
      return skip(ix[:name], 'table_unreadable') if rows.nil?
      return skip(ix[:name], 'index_unreadable') unless clean

      lookup = ix[:kind] == 'btree' || ix[:kind].nil? ? entries.to_set : entries
      found = 0
      rows.each do |key, row|
        derived = begin
          derive_entries(t, definition, key, row)
        rescue Error => e
          add('index_entry_error', object: ix[:name], key: key, detail: "#{e.sqlstate}: #{e.message}")
          next
        end
        derived.each do |entry|
          if index_holds?(ix, lookup, entry)
            found += 1
          else
            add('index_entry_missing', object: ix[:name], key: key, detail: 'no entry for the row')
          end
        end
      end
      return if count == found

      add('index_entry_count_mismatch', object: ix[:name], detail: "index holds #{count} entries, #{found} derive from rows")
    end

    # The engine's form of a stored table and index, which evaluates expression keys and predicates.
    def engine_table(name)
      @catalog_engine.tables.values.find do |table|
        !table[:temporary] && table[:database] == (@database == 'main' ? nil : @database) && (table[:local_name] || table[:name]) == name
      end
    end

    def engine_index(table, name)
      table && table[:indexes].find { |index| (index[:local_name] || index[:name]) == name }
    end

    # The decoded index with evaluators for its expression keys and predicate, or the skip reason its
    # definition and the environment decide (§3.5).
    def resolve_index(t, ix)
      expressions = ix[:predicate] || ix[:cols].any? { |c| c.is_a?(Hash) }
      table = engine_table(t[:name])
      index = engine_index(table, ix[:name])
      return 'unparsable_definition' if index && index[:unparsed]
      return 'missing_host_function' unless (ix[:host_deps] || []).empty? || (index && @engine.send(:host_index_usable?, index))
      return 'missing_loaded_data' if ix[:timezone_deps]

      exported = nil
      if expressions
        return 'unparsable_definition' unless index

        exported = @catalog_engine.send(:export_index, table, index)
        return 'unparsable_definition' if exported[:predicate] && !exported[:where]
        return 'unparsable_definition' if exported[:cols].any? { |c| c.is_a?(Hash) && !c[:evaluate] }
      end
      return 'missing_loaded_data' if index_collations(t, ix, index).any? { |name| collation_unavailable?(name) }

      return ix unless exported

      ix.merge(cols: exported[:cols], where: exported[:where])
    end

    # Collations an index depends on: its bare key columns' and every column its expressions read.
    def index_collations(t, ix, index)
      ordinals = ix[:cols].grep(Integer)
      names = ordinals.map { |ci| t[:columns][ci][:collation] }
      if index
        referenced = column_names([index[:elements], index[:where]])
        names.concat(t[:columns].select { |c| referenced.include?(c[:name]) }.map { |c| c[:collation] })
      end
      names.compact.uniq
    end

    def column_names(node, out = Set.new)
      case node
      when Array then node.each { |child| column_names(child, out) }
      when Hash
        out << node[:name] if node[:kind] == :column && node[:name].is_a?(String)
        node.each_value { |child| column_names(child, out) if child.is_a?(Hash) || child.is_a?(Array) }
      end
      out
    end

    def skewed_collations
      @skewed_collations ||= (@collation_entries || []).filter_map do |entry|
        loaded = @engine.resolve_collation(entry[:name])
        entry[:name] if loaded.nil? || [entry[:unicode], entry[:cldr]] != [loaded.unicode_version, loaded.cldr_version]
      rescue Error
        entry[:name]
      end.to_set
    end

    def collation_unavailable?(name)
      return false if name.nil? || name == 'C' || name.empty?
      return true if skewed_collations.include?(name)

      @engine.resolve_collation(name).nil?
    rescue Error
      true
    end

    # The entries one row derives, in entry order (§3.4).
    def derive_entries(t, ix, key, row)
      case ix[:kind]
      when 'gin'
        terms = []
        ix[:cols].each_with_index do |ci, pos|
          v = row[ci]
          next if v.nil?

          tag = ix[:cols].size > 1 ? [pos].pack('C') : ''.b
          type = t[:columns][ci][:type]
          col_terms = if type == 'jsonb'
                        @codec.gin_jsonb_terms(v)
                      else
                        (v.is_a?(Hash) ? v[:elements] : v).compact.map { |e| @codec.gin_element_term(@codec.array_elem(type), e) }
                      end
          col_terms.map(&:b).uniq.each { |term| terms << (tag + term).b }
        end
        terms.sort.map { |term| [term, key] }
      when 'gist'
        return [] unless @codec.index_admits?(ix, row)
        return [] if ix[:cols].any? { |ci| row[ci].nil? }

        ops = @codec.gist_opclasses_for(t, ix)
        [[@codec.gist_encode_bound(ops, @codec.gist_leaf_bound(ops, row, ix[:cols])).b, key]]
      else
        return [] unless @codec.index_admits?(ix, row)

        [@codec.index_entry_key(t, ix, key, row).b]
      end
    end

    # Whether the index holds a derived entry, found as its lookup finds it.
    def index_holds?(ix, entries, entry)
      case ix[:kind]
      when 'gin'
        term, k = entry
        run = entries.bsearch { |r| r >= term + k }
        return false unless run&.start_with?(term)

        begin
          IntegrityCheck.gin_decode_run(term.bytesize, run).include?(k)
        rescue StandardError
          false
        end
      when 'gist' then entries.include?(entry)
      else entries.include?(entry)
      end
    end

    # --- constraints (§3.4) ---------------------------------------------------------------------------

    def check_constraints(t)
      rows = @tables[t[:name]][:rows]
      named = (t[:checks] || []).map { |c| [c[:name], 0, c] } +
              t[:indexes].select { |ix| ix[:unique] && (ix[:kind].nil? || ix[:kind] == 'btree') }.map { |ix| [ix[:name], 1, ix] } +
              (t[:fks] || []).map { |fk| [fk[:name], 2, fk] }
      named = named.sort_by { |name, kind, _| [name.b, kind] }
      return ([t[:name]] + named.map(&:first)).each { |o| skip(o, 'table_unreadable') } if rows.nil?

      check_columns(t, rows)
      named.each do |_name, kind, c|
        case kind
        when 0 then check_check(t, rows, c)
        when 1 then check_unique(t, c)
        else check_foreign_key(t, rows, c)
        end
      end
    end

    def check_columns(t, rows)
      table = engine_table(t[:name])
      rows.each do |key, row|
        t[:columns].each_with_index do |c, ci|
          v = row[ci]
          if v.nil?
            add('not_null_violation', object: t[:name], key: key, detail: "column #{c[:name]} is NULL") if c[:not_null] || c[:pk] || t[:pk].include?(ci)
          elsif !typmod_fixed?(table, c, ci, v)
            add('typmod_violation', object: t[:name], key: key, detail: "column #{c[:name]} exceeds its typmod")
          end
        end
      end
    end

    # Whether the column's write-time typmod coercion leaves the stored value unchanged.
    def typmod_fixed?(table, c, ci, v)
      case c[:type]
      when 'text'
        c[:varchar_len].nil? || c[:varchar_len].zero? || v.length <= c[:varchar_len]
      when 'decimal'
        return true if c[:precision].nil?

        m = v.match(/\A-?(\d+)(?:\.(\d+))?\z/) or return false
        int_digits = m[1].sub(/\A0+/, '').size
        (m[2] || '').size == c[:scale] && int_digits <= c[:precision] - c[:scale]
      when 'timestamp', 'timestamptz'
        p = c[:temporal]&.dig(:precision)
        return true if p.nil? || v >= 2**63 - 1 || v <= -(2**63)

        unit = 10**(6 - p)
        half = unit / 2
        rounded = v >= 0 ? (v + half) / unit * unit : -((-v + half) / unit * unit)
        rounded == v
      when 'interval'
        return true unless c[:temporal] && table

        column = table[:columns][ci]
        begin
          value = @catalog_engine.send(:from_storage_value, 'interval', v)
          @catalog_engine.send(:storage_value, 'interval', @catalog_engine.send(:assign, value, column.merge(nullable: true))) == v
        rescue Error
          false
        end
      else
        true
      end
    end

    def check_check(t, rows, c)
      table = engine_table(t[:name])
      constraint = table && table[:constraints].find { |item| item[:kind] == :check && item[:name] == c[:name] }
      expr = constraint && constraint[:expr]
      return skip(c[:name], 'unparsable_definition') if expr.nil? || @catalog_engine.send(:opaque_definition?, expr)
      unless (c[:host_deps] || []).empty? || @engine.send(:host_index_usable?, { name: c[:name], host_deps: c[:host_deps] })
        return skip(c[:name], 'missing_host_function')
      end

      referenced = column_names(expr)
      if t[:columns].any? { |col| referenced.include?(col[:name]) && collation_unavailable?(col[:collation]) }
        return skip(c[:name], 'missing_loaded_data')
      end

      compiled = begin
        @catalog_engine.send(:compile, expr, @catalog_engine.send(:table_columns, table))
      rescue Error => e
        return skip(c[:name], resolve_skip(e))
      end
      rows.each do |key, row|
        ok = begin
          @catalog_engine.send(:unmetered_storage_evaluate, compiled, table, row, 'boolean')
        rescue Error => e
          add('check_error', object: c[:name], key: key, detail: "#{e.sqlstate}: #{e.message}")
          next
        end
        add('check_violation', object: c[:name], key: key, detail: 'the row fails the check') if ok == false
      end
    end

    def resolve_skip(error)
      case error.sqlstate
      when 'XX003' then 'unparsable_definition'
      when '42883' then 'missing_host_function'
      when 'XX002' then 'missing_loaded_data'
      else raise error
      end
    end

    # Adjacent entries of a unique index with equal, NULL-free key prefixes.
    def check_unique(t, ix)
      types = if ix[:cols].any? { |c| c.is_a?(Hash) }
                definition = resolve_index(t, ix)
                return skip(ix[:name], definition) if definition.is_a?(String)

                definition[:cols].map { |c| c.is_a?(Hash) ? [c[:type], c[:collation]] : [t[:columns][c][:type], t[:columns][c][:collation]] }
              else
                ix[:cols].map { |c| [t[:columns][c][:type], t[:columns][c][:collation]] }
              end
      return skip(ix[:name], 'index_unreadable') unless @index_clean[ix[:name]]

      orders = @codec.index_orders(ix)
      prev = nil
      @index_entries[ix[:name]].each do |entry|
        split = split_index_entry(entry, types, orders)
        if split.nil?
          prev = nil
          next
        end
        prefix, has_null, skey = split
        add('unique_violation', object: ix[:name], key: skey, detail: "the key repeats the previous entry's") if prev && !has_null && prefix == prev
        prev = has_null ? nil : prefix
      end
    end

    # [key prefix, whether an element is NULL, storage key] of an ordered index entry; nil when the
    # bytes are not an entry.
    def split_index_entry(entry, types, orders)
      pos = 0
      has_null = false
      types.each_with_index do |(type, collation), i|
        present, null = @codec.ordered_slot_tags(*orders[i])
        tag = entry.getbyte(pos)
        return nil if tag.nil?

        pos += 1
        if tag == null
          has_null = true
          next
        end
        return nil unless tag == present

        collation = nil if collation == 'C'
        pos = @codec.skip_key_body(collation ? 'bytea' : type, entry, pos, collation, orders[i][0])
      end
      [entry.byteslice(0, pos), has_null, entry.byteslice(pos..)]
    rescue Error, StandardError
      nil
    end

    def check_foreign_key(t, rows, fk)
      ref = @tables[fk[:ref_table]]
      rt = ref && ref[:table]
      if rt && fk[:ref].any? { |ci| collation_unavailable?(rt[:columns][ci][:collation]) }
        return skip(fk[:name], 'missing_loaded_data')
      end
      return skip(fk[:name], 'table_unreadable') if ref.nil? || ref[:rows].nil?

      by_pk = !rt[:pk].empty? && rt[:pk].sort == fk[:ref].sort
      via = nil
      unless by_pk
        via = rt[:indexes].sort_by { |ix| ix[:name].b }.find do |ix|
          ix[:unique] && (ix[:kind].nil? || ix[:kind] == 'btree') && ix[:cols].all?(Integer) && ix[:cols].sort == fk[:ref].sort
        end
        return skip(fk[:name], 'index_unreadable') if via.nil? || !@index_clean[via[:name]]
      end
      target = via ? via[:cols] : rt[:pk]
      full = (fk[:actions] & 0x40) != 0
      keys = via ? @index_entries[via[:name]] : ref[:rows].map(&:first).to_set
      local_table, parent_table = engine_table(t[:name]), engine_table(rt[:name])
      rows.each do |key, row|
        vals = fk[:local].map { |ci| row[ci] }
        nulls = vals.count(&:nil?)
        next if nulls == vals.size || (nulls.positive? && !full)

        ok = nulls.zero? && begin
          bodies = target.map do |rci|
            i = fk[:ref].index(rci)
            ref_key_body(rt[:columns][rci], t[:columns][fk[:local][i]], vals[i], parent_table, local_table)
          end
          if bodies.include?(nil)
            false
          elsif via
            orders = @codec.index_orders(via)
            prefix = bodies.each_with_index.map do |b, i|
              b = b.bytes.map { |x| x ^ 0xFF }.pack('C*') if orders[i][0]
              @codec.ordered_slot_tags(*orders[i])[0].chr.b + b
            end.join.b
            start = keys.bsearch_index { |e| e >= prefix }
            !start.nil? && keys[start].start_with?(prefix)
          else
            keys.include?(bodies.join.b)
          end
        end
        add('foreign_key_violation', object: fk[:name], key: key, detail: 'no referenced row') unless ok
      end
    end

    # The referenced column's key body for a referencing value, converted to the referenced type as
    # at write time; nil when no row can hold it.
    def ref_key_body(ref_col, local_col, v, parent_table, local_table)
      if ref_col[:type] == local_col[:type]
        return key_body(ref_col, v)
      end

      converted = begin
        value = @catalog_engine.send(:from_storage_value, local_col[:type], v)
        target = parent_table && parent_table[:columns].find { |c| c[:name] == ref_col[:name] }
        cast = @catalog_engine.send(:cast_value, value, target ? target[:type] : ref_col[:type], source_type: local_col[:type])
        @catalog_engine.send(:storage_value, ref_col[:type], cast)
      rescue Error
        return nil
      end
      key_body(ref_col, converted)
    rescue Error, StandardError
      nil
    end

    # --- space accounting (§3.3) ----------------------------------------------------------------------

    def check_free_list
      entries = []
      if @durable
        chain = []
        page = @meta[:free_head]
        until page.zero?
          if page < @first || page >= @meta[:page_count] || chain.include?(page)
            add('free_list_corrupt', page: page, detail: "free-list chain names page #{page}")
            break
          end
          add('free_page_live', page: page, detail: "free-list page #{page} is reachable") if @reached.include?(page)
          begin
            pg = read_page(page)
            raise 'not a free-list page' unless pg[:type] == 7
            raise 'invalid free-list count' if pg[:item_count] > pg[:payload].bytesize / 4
          rescue Error, StandardError => e
            raise unless catalog_error?(e)

            add('free_list_corrupt', page: page, detail: e.message)
            break
          end
          chain << page
          @free_chain_pages += 1
          entries.concat(pg[:payload].byteslice(0, pg[:item_count] * 4).unpack('N*'))
          page = pg[:next_page]
        end
      else
        entries = (@memory_free || []).dup
      end
      protected_pages = (@meta[:protected] || []).to_set
      seen = Set.new
      entries.each do |p|
        if p < @first || p >= @meta[:page_count]
          add('free_page_out_of_range', page: p, detail: "free page #{p} outside [#{@first}, #{@meta[:page_count]})")
        elsif !seen.add?(p)
          add('free_page_duplicate', page: p, detail: "free page #{p} listed twice")
        elsif @reached.include?(p)
          add('free_page_live', page: p, detail: "free page #{p} is reachable")
        elsif protected_pages.include?(p)
          add('free_page_manifest_dependency', page: p, detail: "free page #{p} is a dependency of the generation")
        end
      end
    end

    public

    attr_reader :codec

    def page_ok?(object, page) = reach(object, page)
    def finding(kind, **args) = add(kind, **args)
    def page(index) = read_page(index)
    def decode_error?(error) = catalog_error?(error)

    # Walk one B+tree depth-first in child order, checking page verification, node types, uniform leaf
    # depth, strict key order within a node and separator bounds. Yields each leaf record in key order.
    # Returns false when part of the tree could not be read (§5.2).
    class BtreeWalk
      attr_reader :postings

      def initialize(check, object, columns, gin: false)
        @check, @object, @columns, @gin = check, object, columns, gin
        @leaf_depth = nil
        @readable = true
        @postings = 0
      end

      def run(root, &block)
        @block = block
        visit(root, 0, nil, nil) unless root.zero?
        @readable
      end

      private

      def visit(page, depth, lo, hi)
        unless @check.page_ok?(@object, page)
          @readable = false
          return
        end
        return corrupt(page, 'deeper than any valid tree') if depth >= MAX_TREE_DEPTH

        pg = begin
          @check.page(page)
        rescue Error, StandardError => e
          raise unless @check.decode_error?(e)

          return corrupt(page, e.message)
        end
        case pg[:type]
        when FileFormat::PAGE_INTERIOR
          begin
            children, seps = parse_interior(pg)
          rescue Error, StandardError => e
            raise unless @check.decode_error?(e)

            return corrupt(page, e.message)
          end
          check_keys(page, seps, lo, hi)
          children.each_with_index do |child, i|
            visit(child, depth + 1, i.zero? ? lo : seps[i - 1], i == seps.size ? hi : seps[i])
          end
        when FileFormat::PAGE_LEAF
          @leaf_depth ||= depth
          if depth != @leaf_depth
            @check.finding('tree_depth_uneven', object: @object, page: page, detail: "leaf at depth #{depth}, others at #{@leaf_depth}")
          end
          begin
            keys, regions = @check.codec.parse_pax_leaf(pg[:payload], pg[:item_count], @columns || [])
          rescue Error, StandardError => e
            raise unless @check.decode_error?(e)

            return corrupt(page, e.message)
          end
          keys = keys.map(&:b)
          check_keys(page, keys, lo, hi)
          if @gin
            begin
              @postings += keys.sum { |key| IntegrityCheck.gin_run_postings(key) }
            rescue Error, StandardError => e
              raise unless @check.decode_error?(e)

              return corrupt(page, e.message)
            end
          end
          keys.each_with_index do |key, i|
            values = (@columns || []).each_with_index.map { |c, ci| [c[:type], raw_value(regions[ci], i)] }
            @block&.call(page, key, values)
          end
        else
          @readable = false
          @check.finding('page_type_wrong', object: @object, page: page, detail: "page type #{pg[:type]} in a B-tree")
        end
      end

      def corrupt(page, message)
        @readable = false
        @check.finding('page_corrupt', object: @object, page: page, detail: message)
      end

      # At most one keys_unordered and one separator_violation per page.
      def check_keys(page, keys, lo, hi)
        unless keys.each_cons(2).all? { |a, b| (a <=> b) == -1 }
          @check.finding('keys_unordered', object: @object, page: page, detail: 'keys do not ascend strictly')
        end
        return if keys.all? { |k| (lo.nil? || (k <=> lo) >= 0) && (hi.nil? || (k <=> hi) == -1) }

        @check.finding('separator_violation', object: @object, page: page, detail: 'a key lies outside the bounds its parent gives it')
      end

      def parse_interior(pg)
        n = pg[:item_count]
        payload = pg[:payload]
        raise 'interior directory overruns page' if 4 * (n + 1) + 4 * n > payload.bytesize

        children = payload.byteslice(0, 4 * (n + 1)).unpack('N*')
        ends = payload.byteslice(4 * (n + 1), 4 * n).unpack('N*')
        pos = 8 * n + 4
        raise 'interior separator directory not ascending' unless ends.each_cons(2).all? { |a, b| a <= b }
        raise 'interior key blob overruns page' if pos + (ends.last || 0) > payload.bytesize

        seps = ends.each_with_index.map do |e, i|
          from = i.zero? ? 0 : ends[i - 1]
          payload.byteslice(pos + from, e - from).b
        end
        [children, seps]
      end

      # A value's stored bytes in a parsed leaf region, re-tagged for decode_value; nil for NULL.
      def raw_value(region, i)
        if region[:width]
          w = region[:width]
          return nil if (region[:bitmap].getbyte(i / 8) & (0x80 >> (i % 8))) != 0

          "\x00".b + region[:body].byteslice(i * w, w)
        else
          from = i.zero? ? 0 : region[:ends][i - 1]
          len = region[:ends][i] - from
          len.zero? ? nil : region[:body].byteslice(from, len)
        end
      end
    end

    # Walk a GiST R-tree: page verification, node types, uniform leaf depth, every bound decoding under
    # the index's opclasses, and each interior bound containing every bound in its child. Collects the
    # leaf entries as [bound bytes, storage key].
    class GistWalk
      attr_reader :entries

      def initialize(check, object, ops)
        @check, @object, @ops = check, object, ops
        @leaf_depth = nil
        @readable = true
        @entries = []
      end

      def run(root)
        visit(root, 0, nil) unless root.zero?
        @readable
      end

      private

      def visit(page, depth, parent_bound)
        unless @check.page_ok?(@object, page)
          @readable = false
          return
        end
        if depth >= MAX_TREE_DEPTH
          @readable = false
          return @check.finding('page_corrupt', object: @object, page: page, detail: 'deeper than any valid tree')
        end
        begin
          pg = @check.page(page)
          items = parse(pg)
        rescue Error, StandardError => e
          raise unless @check.decode_error?(e)

          @readable = false
          return @check.finding('page_corrupt', object: @object, page: page, detail: e.message)
        end
        if items.nil?
          @readable = false
          return @check.finding('page_type_wrong', object: @object, page: page, detail: "page type #{pg[:type]} in a GiST tree")
        end
        leaf = pg[:type] == FileFormat::PAGE_GIST_LEAF
        if leaf
          @leaf_depth ||= depth
          if depth != @leaf_depth
            @check.finding('tree_depth_uneven', object: @object, page: page, detail: "leaf at depth #{depth}, others at #{@leaf_depth}")
          end
        end
        if parent_bound && !items.all? { |bound, _, _| IntegrityCheck.gist_bound_contains?(@check.codec, @ops, parent_bound, bound) }
          @check.finding('gist_bounds_violation', object: @object, page: page, detail: "a bound lies outside its parent's bound for this page")
        end
        if leaf
          items.each { |_, raw, skey| @entries << [raw, skey] }
        else
          items.each { |bound, _, child| visit(child, depth + 1, bound) }
        end
      end

      # [decoded bound, bound bytes, storage key or child page] per entry; nil for another page type.
      def parse(pg)
        return nil unless [FileFormat::PAGE_GIST_LEAF, FileFormat::PAGE_GIST_INTERIOR].include?(pg[:type])

        codec = @check.codec
        payload = pg[:payload]
        pos = 0
        (0...pg[:item_count]).map do
          bl, pos = codec.take(payload, pos, 2)
          raw, pos = codec.take(payload, pos, bl.unpack1('n'))
          bound = IntegrityCheck.gist_decode_bound(codec, @ops, raw.b)
          if pg[:type] == FileFormat::PAGE_GIST_LEAF
            sl, pos = codec.take(payload, pos, 2)
            skey, pos = codec.take(payload, pos, sl.unpack1('n'))
            [bound, raw.b, skey.b]
          else
            cp, pos = codec.take(payload, pos, 4)
            [bound, raw.b, cp.unpack1('N')]
          end
        end
      end
    end

    # --- GIN posting runs and GiST bounds (§3.2, §3.4) -----------------------------------------------

    def self.gin_read_run_varint(buf, pos)
      v = 0
      [0, 7, 14].each do |shift|
        raise 'a GIN posting run is malformed' if pos >= buf.bytesize

        c = buf.getbyte(pos)
        pos += 1
        v |= (c & 0x7F) << shift
        return [v, pos] if (c & 0x80).zero?
      end
      raise 'a GIN posting run is malformed'
    end

    # A run's posting count from its tail, without the term's length.
    def self.gin_run_postings(run)
      n = run.bytesize
      raise 'a GIN posting run is malformed' if n < 4

      body_len = run.byteslice(n - 2, 2).unpack1('n')
      raise 'a GIN posting run is malformed' if n - 2 - body_len < 2

      body = run.byteslice(n - 2 - body_len, body_len)
      at = 0
      count = 1
      while at < body.bytesize
        shared, at = gin_read_run_varint(body, at)
        suffix, at = gin_read_run_varint(body, at)
        raise 'a GIN posting run is malformed' if at + suffix > body.bytesize || (shared + suffix).zero?

        at += suffix
        count += 1
      end
      count
    end

    # A run's storage keys, ascending, given its term's length (gin.md §4.5).
    def self.gin_decode_run(term_len, run)
      n = run.bytesize
      raise 'a GIN posting run is malformed' if n < term_len + 3

      body_len = run.byteslice(n - 2, 2).unpack1('n')
      last_end = n - 2 - body_len
      raise 'a GIN posting run is malformed' if last_end <= term_len

      last = run.byteslice(term_len, last_end - term_len)
      body = run.byteslice(last_end, body_len)
      keys = []
      at = 0
      while at < body.bytesize
        shared, at = gin_read_run_varint(body, at)
        suffix, at = gin_read_run_varint(body, at)
        prev = keys.last || last
        raise 'a GIN posting run is malformed' if shared > prev.bytesize || at + suffix > body.bytesize || (shared + suffix).zero?

        key = prev.byteslice(0, shared) + body.byteslice(at, suffix)
        at += suffix
        raise 'a GIN posting run is malformed' if keys.last && keys.last >= key

        keys << key
      end
      raise 'a GIN posting run is malformed' if keys.last && keys.last >= last

      keys << last
    end

    # A GiST tuple bound: per opclass a range body or a [min, max] pair of length-prefixed key bytes.
    def self.gist_decode_bound(codec, ops, raw)
      pos = 0
      bound = ops.map do |op|
        if op[:kind] == :range
          v, pos = codec.decode_range_body(op[:elem], raw, pos)
          v
        else
          ml, pos = codec.take(raw, pos, 2)
          min, pos = codec.take(raw, pos, ml.unpack1('n'))
          xl, pos = codec.take(raw, pos, 2)
          max, pos = codec.take(raw, pos, xl.unpack1('n'))
          [min.b, max.b]
        end
      end
      raise 'GiST bound has trailing bytes' unless pos == raw.bytesize

      bound
    end

    # Whether a decoded bound contains another, per component: range @> through the order-preserving
    # bound encodings, scalar [min, max] bytewise.
    def self.gist_bound_contains?(codec, ops, outer, inner)
      ops.each_index.all? do |k|
        a = outer[k]
        b = inner[k]
        if ops[k][:kind] == :range
          next true if b == :empty
          next false if a == :empty

          elem = ops[k][:elem]
          bound = ->(v, side, lower) { codec.encode_range_key_bound(+''.b, elem, v, side, lower) }
          bound.(a, :lower, true) <= bound.(b, :lower, true) && bound.(b, :upper, false) <= bound.(a, :upper, false)
        else
          a[0] <= b[0] && b[1] <= a[1]
        end
      end
    end
  end

  # Database#integrity_check (spec/design/integrity-check.md §2): checks `main` or an attachment on
  # this handle, pinned like a read session, without taking the writer gate.
  module IntegrityOperations
    def integrity_check(name = 'main', level: :full, constraints: false, max_findings: 100)
      name = String(name)
      reader = read_session
      target = nil
      @shared_mutex.synchronize do
        shared_open!
        attachment = name == 'main' ? nil : @engine.instance_variable_get(:@attachments)[name]
        raise Error.new('42704', 'database is not attached') unless name == 'main' || attachment

        storage = attachment ? attachment[:storage] : @engine.instance_variable_get(:@storage)
        target = if storage
                   storage.load
                   meta = storage.meta.dup
                   image = storage.instance_variable_get(:@image)
                   reread = meta[:slot] && storage.instance_variable_get(:@codec).read_meta(image, storage.page_size, meta[:slot], validate_body: false)
                   meta = meta.merge(reread.slice(:manifest, :manifest_pages, :protected)) if reread && reread[:txid] == meta[:txid]
                   { image: image, page_size: storage.page_size, meta: meta, durable: true, crypto: storage.crypto }
                 else
                   memory_integrity_target(name)
                 end
      end
      IntegrityCheck.new(database: name, engine: reader.engine, catalog_engine: reader.engine,
                         level: level, constraints: constraints, max_findings: max_findings, **target).run
    ensure
      reader&.close
    end

    private

    # An in-memory domain: its pages, catalog root and high-water, and the free list it keeps in RAM
    # (no live count, manifest or persisted free list — §2).
    def memory_integrity_target(name)
      domain = @engine.instance_variable_get(:@memory_domains)[name]
      codec = @engine.send(:statistics_codec)
      unless domain
        page_size = name == 'main' ? page_size() : 8192
        image = codec.build_image([], [], [], page_size)
        meta = codec.select_meta(image, page_size)
        return { image: image, page_size: page_size, meta: meta.merge(protected: []), durable: false, memory_free: [] }
      end
      pages = domain.instance_variable_get(:@pages)
      meta = { root_page: domain.root, page_count: domain.page_count, free_head: 0, live: nil, protected: [] }
      { image: IntegrityPagesImage.new(pages, domain.page_size, domain.page_count), page_size: domain.page_size,
        meta: meta, durable: false, memory_free: domain.instance_variable_get(:@free).dup }
    end
  end
  Database.prepend(IntegrityOperations)
end
