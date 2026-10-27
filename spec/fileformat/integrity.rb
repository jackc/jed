# The integrity-check reference (spec/design/integrity-check.md): an independent fourth voice for the
# report every core's `integrity_check` returns. Loaded by verify.rb, whose decoder helpers it reuses
# (meta selection, page reads, leaf and value decoding, key encoding), and run as
#
#   ruby spec/fileformat/verify.rb --integrity-check [--quick] [--max-findings N] FILE…
#
# It checks a file as a freshly opened handle would see it: meta selection, the catalog, and the free
# list's chain must load (as `open` requires), and everything after that is a finding, not an error.
# It covers the checks it can evaluate without SQL. Index agreement for GIN, GiST, expression and
# partial indexes, and GiST bounding boxes, are listed under `reference_unchecked` (not part of the
# cross-core report) until slice I3 teaches this reference to derive them.

require "json"

INTEGRITY_DIR = File.join(__dir__, "integrity")
INTEGRITY_REGISTRY = File.join(__dir__, "..", "integrity", "findings.toml")

# A page at this depth or deeper (the root is depth 0) is page_corrupt without being read: no valid
# tree over u32 pages comes near it, and the cores walk with bounded native stacks
# (integrity-check.md §5.2).
INTEGRITY_MAX_TREE_DEPTH = 256

class IntegrityCheck
  # Raised by the overflow fetch when a chain names a page the walk already reached.
  class ReachedTwice < StandardError
    attr_reader :page

    def initialize(page)
      @page = page
      super("page #{page} reached twice")
    end
  end

  # Raised by the overflow fetch when a chain names a page outside the body range.
  class OutOfRange < StandardError
    attr_reader :page

    def initialize(page)
      @page = page
      super("page #{page} out of range")
    end
  end

  def initialize(image, level: "full", max_findings: 100)
    @image = image
    @level = level
    @max = max_findings
    @findings = []
    @skipped = []
    @unchecked = []
    @truncated = false
    @reached = Set.new
    @rows = 0
    @damaged = false
    @free_chain_pages = 0
  end

  def run
    # verify.rb's decoder bookkeeping; a decode that raised may have left it set.
    $reached = nil
    $claimed = nil
    @ps = @image.byteslice(8, 4).unpack1("N")
    @meta = select_meta(@image, @ps)
    unlock_image(@image, @ps, @meta) if @meta[:suite] == 1
    @first = first_body
    catalog = load_catalog
    catch(:truncated) do
      check_sequences(catalog[:sequences])
      catalog[:tables].sort_by { |t| t[:name].b }.each { |t| check_table(t) }
      check_free_list
      if !@damaged && @meta[:live] != @reached.size
        add("live_count_mismatch", detail: "meta records #{@meta[:live]} live pages, #{@reached.size} are reachable")
      end
    end
    { findings: @findings, truncated: @truncated, skipped: @skipped,
      pages: @reached.size + @free_chain_pages, rows: @rows, reference_unchecked: @unchecked }
  ensure
    $enc = nil
    $ctypes = nil
  end

  private

  def add(kind, object: nil, page: nil, key: nil, detail: "")
    if !@max.zero? && @findings.size == @max
      @truncated = true
      throw :truncated
    end
    @findings << { kind: kind, database: "main", object: object, page: page, key: key&.unpack1("H*"), detail: detail }
  end

  # The catalog as `open` loads it: any failure here is an open failure, not a finding (the check runs
  # on an open handle, integrity-check.md §1). Catalog pages are live, so they are reached first.
  def load_catalog
    types = []
    sequences = []
    tables = []
    statistics = []
    statistics_expected = {}
    $ctypes = {}
    cat = @meta[:root_page]
    while cat != 0
      raise "catalog page #{cat} reached twice" unless @reached.add?(cat)

      pg = read_page(@image, @ps, cat)
      raise "expected a catalog page" unless pg[:type] == PAGE_CATALOG && (pg[:flags] & ~1).zero?

      nxt = pg[:next_page]
      payload = pg[:payload]
      if pg[:flags] & 1 == 1
        payload = payload.dup
        loop do
          raise "a continued catalog entry ends the chain" if nxt.zero?
          raise "catalog page #{nxt} reached twice" unless @reached.add?(nxt)

          cp = read_page(@image, @ps, nxt)
          payload << cp[:payload]
          nxt = cp[:next_page]
          break if (cp[:flags] & 1).zero?
        end
      end
      pos = 0
      pg[:item_count].times do
        kb, pos = take(payload, pos, 1)
        case kb.getbyte(0)
        when 0
          entry, pos = decode_table_entry(payload, pos)
          tables << entry
        when 1
          ct, pos = decode_composite_type_entry(payload, pos)
          types << ct
          $ctypes[ct[:name]] = ct[:fields]
        when 2
          s, pos = decode_sequence_entry(payload, pos)
          sequences << s
        when 3
          _c, pos = decode_collation_entry(payload, pos)
        when 4
          pos = decode_statistics_entry(payload, pos, tables, statistics, statistics_expected)
        when 5
          _v, pos = decode_view_entry(payload, pos)
        else
          raise "unknown catalog entry kind"
        end
      end
      cat = nxt
    end
    { types: types, sequences: sequences, tables: tables }
  end

  def check_sequences(sequences)
    sequences.sort_by { |s| s[:name].b }.each do |s|
      next if s[:last_value].between?(s[:min_value], s[:max_value])

      add("sequence_out_of_range", object: s[:name],
                                   detail: "value #{s[:last_value]} outside [#{s[:min_value]}, #{s[:max_value]}]")
    end
  end

  # Record a page as reached. Returns false (after reporting) when the walk must not read it.
  def reach(object, page)
    unless page >= @first && page < @meta[:page_count]
      add("page_out_of_range", object: object, page: page,
                               detail: "page #{page} outside [#{@first}, #{@meta[:page_count]})")
      return false
    end
    unless @reached.add?(page)
      add("page_reached_twice", object: object, page: page, detail: "page #{page} is already part of the database")
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
      add("row_count_mismatch", object: t[:name],
                                detail: "catalog records #{t[:row_count]} rows, the tree holds #{rows.size}")
    end
    @damaged ||= damaged
    t[:indexes].sort_by { |ix| ix[:name].b }.each { |ix| check_index(t, ix, damaged ? nil : rows) }
  end

  # Decode one row; nil when a value does not decode (the row's table is then unreadable). An
  # overflow pointer out of range or already reached names that page; any other failure of an
  # external value is its chain's, reported at the row's leaf.
  def check_row(t, page, key, values)
    @rows += 1
    row = values.each_with_index.map do |(type, raw), ci|
      next nil if raw.nil?

      column = "column #{t[:columns][ci][:name]}"
      begin
        v, = decode_value(type, raw, 0, overflow_fetch)
        v
      rescue ReachedTwice, OutOfRange => e
        kind = e.is_a?(ReachedTwice) ? "page_reached_twice" : "page_out_of_range"
        add(kind, object: t[:name], page: e.page, key: key, detail: "#{column}: #{e.message}")
        return nil
      rescue StandardError => e
        external = [TAG_EXTERNAL, TAG_EXTERNAL_COMP].include?(raw.getbyte(0))
        kind = external ? "overflow_chain_corrupt" : "value_undecodable"
        add(kind, object: t[:name], page: page, key: key, detail: "#{column}: #{e.message}")
        return nil
      end
    end
    unless t[:pk].empty?
      # A key column holding a value its key encoding rejects (a NULL) cannot be this row's key either.
      want = begin
        t[:pk].map { |pi| key_body(t[:columns][pi][:type], row[pi], t[:columns][pi][:collation]) }.join.b
      rescue StandardError
        nil
      end
      unless want == key
        add("row_key_not_primary_key", object: t[:name], page: page, key: key,
                                       detail: "stored key is not the row's primary-key encoding")
      end
    end
    [key, row]
  end

  def overflow_fetch
    lambda do |p|
      raise OutOfRange, p unless p >= @first && p < @meta[:page_count]
      raise ReachedTwice, p unless @reached.add?(p)

      read_page(@image, @ps, p)
    end
  end

  # An index whose walk reported anything, even an order or bounds finding, is not checked for
  # agreement: its own lookups cannot be trusted to find what it holds (integrity-check.md §5.2).
  def check_index(t, ix, rows)
    before = @findings.size
    if ix[:kind] == "gist"
      walk = GistWalk.new(self, ix[:name])
      readable = walk.run(ix[:root_page])
      @unchecked << { object: ix[:name], check: "gist_bounds" }
      entries = nil
    else
      entries = []
      walk = BtreeWalk.new(self, ix[:name], nil)
      readable = walk.run(ix[:root_page]) { |_page, key, _values| entries << key }
    end
    @damaged ||= !readable
    return unless @level == "full"
    return skip(ix[:name], "table_unreadable") if rows.nil?
    return skip(ix[:name], "index_unreadable") unless readable && @findings.size == before

    derivable = ix[:kind] == "btree" && ix[:predicate].nil? && ix[:cols].all?(Integer)
    return @unchecked << { object: ix[:name], check: "index_agreement" } unless derivable

    present = entries.to_set
    found = 0
    rows.each do |key, row|
      want = index_entry_key(t, ix, key, row)
      if present.include?(want)
        found += 1
      else
        add("index_entry_missing", object: ix[:name], key: key, detail: "no entry for the row")
      end
    end
    return if entries.size == found

    add("index_entry_count_mismatch", object: ix[:name],
                                      detail: "index holds #{entries.size} entries, #{found} derive from rows")
  end

  def skip(object, reason)
    @skipped << { object: object, reason: reason }
  end

  public

  attr_reader :image, :ps

  def page_ok?(object, page) = reach(object, page)

  def finding(kind, **args) = add(kind, **args)

  # Walk one B+tree depth-first in child order (format.md *Interior node*), checking page
  # verification, node types, uniform leaf depth, strict key order within a node, and that every key
  # lies in the bounds its parent's separators give it. Yields each leaf record in key order. Returns
  # false when any part of the tree could not be read (integrity-check.md §5.2).
  class BtreeWalk
    def initialize(check, object, columns)
      @check = check
      @object = object
      @columns = columns
      @leaf_depth = nil
      @readable = true
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
      return corrupt(page, "deeper than any valid tree") if depth >= INTEGRITY_MAX_TREE_DEPTH

      begin
        pg = read_page(@check.image, @check.ps, page)
      rescue StandardError => e
        return corrupt(page, e.message)
      end
      case pg[:type]
      when PAGE_INTERIOR
        begin
          children, seps = parse_interior(pg)
        rescue StandardError => e
          return corrupt(page, e.message)
        end
        check_keys(page, seps, lo, hi)
        children.each_with_index do |child, i|
          visit(child, depth + 1, i.zero? ? lo : seps[i - 1], i == seps.size ? hi : seps[i])
        end
      when PAGE_LEAF
        @leaf_depth ||= depth
        if depth != @leaf_depth
          @check.finding("tree_depth_uneven", object: @object, page: page,
                                              detail: "leaf at depth #{depth}, others at #{@leaf_depth}")
        end
        begin
          keys, regions = parse_pax_leaf(pg[:payload], pg[:item_count], @columns || [])
        rescue StandardError => e
          return corrupt(page, e.message)
        end
        keys = keys.map(&:b)
        check_keys(page, keys, lo, hi)
        keys.each_with_index do |key, i|
          values = (@columns || []).each_with_index.map { |c, ci| [c[:type], raw_value(regions[ci], i)] }
          @block&.call(page, key, values)
        end
      else
        @readable = false
        @check.finding("page_type_wrong", object: @object, page: page,
                                          detail: "page type #{pg[:type]} in a B-tree")
      end
    end

    def corrupt(page, message)
      @readable = false
      @check.finding("page_corrupt", object: @object, page: page, detail: message)
    end

    # At most one keys_unordered and one separator_violation per page.
    def check_keys(page, keys, lo, hi)
      unless keys.each_cons(2).all? { |a, b| (a <=> b) == -1 }
        @check.finding("keys_unordered", object: @object, page: page, detail: "keys do not ascend strictly")
      end
      return if keys.all? { |k| (lo.nil? || (k <=> lo) >= 0) && (hi.nil? || (k <=> hi) == -1) }

      @check.finding("separator_violation", object: @object, page: page,
                                            detail: "a key lies outside the bounds its parent gives it")
    end

    def parse_interior(pg)
      n = pg[:item_count]
      payload = pg[:payload]
      pos = 0
      children = (0..n).map do
        cp, pos = read_ptr(payload, pos)
        cp
      end
      ends = (0...n).map do
        e, pos = take(payload, pos, 4)
        e.unpack1("N")
      end
      raise "interior separator directory not ascending" unless ends.each_cons(2).all? { |a, b| a <= b }
      raise "interior key blob overruns page" if pos + (ends.last || 0) > payload.bytesize

      seps = ends.each_with_index.map do |e, i|
        lo = i.zero? ? 0 : ends[i - 1]
        payload.byteslice(pos + lo, e - lo).b
      end
      [children, seps]
    end

    # The stored bytes of record i's value in a parsed leaf region, re-tagged for decode_value (a
    # fixed slot is untagged on the page); nil for NULL.
    def raw_value(region, i)
      if region[:width]
        w = region[:width]
        return nil if (region[:bitmap].getbyte(i / 8) & (0x80 >> (i % 8))) != 0

        "\x00".b + region[:body].byteslice(i * w, w)
      else
        lo = i.zero? ? 0 : region[:ends][i - 1]
        len = region[:ends][i] - lo
        len.zero? ? nil : region[:body].byteslice(lo, len)
      end
    end
  end

  # Walk a GiST R-tree (gist.md §4.1) for page verification, node types, and uniform leaf depth.
  # Bounding-box containment needs each opclass's union and is left to slice I3.
  class GistWalk
    def initialize(check, object)
      @check = check
      @object = object
      @leaf_depth = nil
      @readable = true
    end

    def run(root)
      visit(root, 0) unless root.zero?
      @readable
    end

    private

    def visit(page, depth)
      unless @check.page_ok?(@object, page)
        @readable = false
        return
      end
      if depth >= INTEGRITY_MAX_TREE_DEPTH
        @readable = false
        return @check.finding("page_corrupt", object: @object, page: page, detail: "deeper than any valid tree")
      end
      begin
        pg = read_page(@check.image, @check.ps, page)
        children = parse(pg)
      rescue StandardError => e
        @readable = false
        return @check.finding("page_corrupt", object: @object, page: page, detail: e.message)
      end
      if children.nil?
        @readable = false
        return @check.finding("page_type_wrong", object: @object, page: page,
                                                 detail: "page type #{pg[:type]} in a GiST tree")
      end
      if pg[:type] == PAGE_GIST_LEAF
        @leaf_depth ||= depth
        return if depth == @leaf_depth

        return @check.finding("tree_depth_uneven", object: @object, page: page,
                                                   detail: "leaf at depth #{depth}, others at #{@leaf_depth}")
      end
      children.each { |cp| visit(cp, depth + 1) }
    end

    # The child pointers of an interior, [] for a leaf (its entries parsed and discarded), nil for
    # another page type.
    def parse(pg)
      payload = pg[:payload]
      pos = 0
      case pg[:type]
      when PAGE_GIST_LEAF
        pg[:item_count].times do
          bl, pos = take(payload, pos, 2)
          _bound, pos = take(payload, pos, bl.unpack1("n"))
          sl, pos = take(payload, pos, 2)
          _skey, pos = take(payload, pos, sl.unpack1("n"))
        end
        []
      when PAGE_GIST_INTERIOR
        (0...pg[:item_count]).map do
          bl, pos = take(payload, pos, 2)
          _bound, pos = take(payload, pos, bl.unpack1("n"))
          cp, pos = read_ptr(payload, pos)
          cp
        end
      end
    end
  end

  private

  # The free list as the space-accounting check sees it (integrity-check.md §3.3). Each chain page
  # counts toward `pages` once it is read and verified.
  def check_free_list
    chain = []
    page = @meta[:free_head]
    entries = []
    until page.zero?
      if page < @first || page >= @meta[:page_count] || chain.include?(page)
        add("free_list_corrupt", page: page, detail: "free-list chain names page #{page}")
        break
      end
      add("free_page_live", page: page, detail: "free-list page #{page} is reachable") if @reached.include?(page)
      begin
        pg = read_page(@image, @ps, page)
        raise "not a free-list page" unless pg[:type] == 7
        raise "invalid free-list count" if pg[:item_count] > pg[:payload].bytesize / 4
      rescue StandardError => e
        add("free_list_corrupt", page: page, detail: e.message)
        break
      end
      chain << page
      @free_chain_pages += 1
      entries.concat(pg[:payload].byteslice(0, pg[:item_count] * 4).unpack("N*"))
      page = pg[:next_page]
    end
    protected = (@meta[:protected] || []).to_set
    seen = Set.new
    entries.each do |p|
      if p < @first || p >= @meta[:page_count]
        add("free_page_out_of_range", page: p, detail: "free page #{p} outside [#{@first}, #{@meta[:page_count]})")
      elsif !seen.add?(p)
        add("free_page_duplicate", page: p, detail: "free page #{p} listed twice")
      elsif @reached.include?(p)
        add("free_page_live", page: p, detail: "free page #{p} is reachable")
      elsif protected.include?(p)
        add("free_page_manifest_dependency", page: p, detail: "free page #{p} is a dependency of the generation")
      end
    end
    chain
  end
end

# The cross-core part of a report (integrity-check.md §4): everything but `detail` and the
# reference's own `reference_unchecked` list.
def integrity_contract(report)
  { findings: report[:findings].map { |f| f.except(:detail) }, truncated: report[:truncated],
    skipped: report[:skipped], pages: report[:pages], rows: report[:rows] }
end

def integrity_check_files(args)
  level = args.delete("--quick") ? "quick" : "full"
  max = 100
  if (i = args.index("--max-findings"))
    max = Integer(args[i + 1])
    args.slice!(i, 2)
  end
  failed = false
  args.each do |path|
    report = IntegrityCheck.new(File.binread(path), level: level, max_findings: max).run
    failed ||= !report[:findings].empty?
    puts JSON.generate({ file: path, **report })
  rescue StandardError => e
    failed = true
    puts JSON.generate({ file: path, open_failed: e.message })
  end
  exit(failed ? 1 : 0)
end

# --- damaged fixtures (integrity-check.md §6) --------------------------------
#
# Each fixture is a from-scratch golden with one deterministic damage, its expected reports beside it
# (`<name>.json`, one per configuration). Semantic damage recomputes the page or meta checksum, so the
# logical checks, not the checksum, must catch it. Every damage leaves a file that opens: no catalog,
# free-list chain, meta, or first-child interior page is touched (open reads those).

# 40 rows under even ids, so a row can be re-keyed into a gap without breaking key order. At a
# 256-byte page the table and its index are each an interior root over several leaves.
INTEGRITY_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("v", "i32"), col("note", "text")],
  indexes: [{ name: "t_v_idx", cols: [1] }],
  rows: (1..40).map { |i| [i * 2, (i * 37) % 101, "row #{i}"] }
}.freeze

INTEGRITY_DEFAULT_RUNS = [{ level: "quick", max_findings: 100 }, { level: "full", max_findings: 100 }].freeze

INTEGRITY_FIXTURES = [
  { name: "clean", damage: nil },
  { name: "leaf_checksum", damage: :leaf_checksum },
  { name: "leaf_keys_swapped", damage: :leaf_keys_swapped,
    runs: INTEGRITY_DEFAULT_RUNS + [{ level: "full", max_findings: 2 }] },
  { name: "row_rekeyed", damage: :row_rekeyed },
  { name: "index_entry_dropped", damage: :index_entry_dropped },
  { name: "index_entry_extra", damage: :index_entry_extra },
  { name: "index_keys_swapped", damage: :index_keys_swapped },
  { name: "row_count_wrong", damage: :none, table: INTEGRITY_TABLE.merge(row_count: 41) },
  { name: "live_count_wrong", damage: :live_count_wrong },
  { name: "free_page_live", damage: :free_page_live },
  { name: "tree_shares_page", damage: :tree_shares_page }
].freeze

# Table and index root pages by name, from a plaintext from-scratch catalog of tables only.
def integrity_roots(image, ps)
  roots = {}
  cat = select_meta(image, ps)[:root_page]
  while cat != 0
    pg = read_page(image, ps, cat)
    pos = 0
    pg[:item_count].times do
      raise "integrity fixtures carry only tables" unless pg[:payload].getbyte(pos).zero?

      entry, pos = decode_table_entry(pg[:payload], pos + 1)
      roots[entry[:name]] = entry[:root_data_page]
      entry[:indexes].each { |ix| roots[ix[:name]] = ix[:root_page] }
    end
    cat = pg[:next_page]
  end
  roots
end

# The leaf pages of a plaintext B+tree, in key order.
def integrity_leaves(image, ps, root)
  pg = read_page(image, ps, root)
  return [root] if pg[:type] == PAGE_LEAF

  (0..pg[:item_count]).flat_map { |i| integrity_leaves(image, ps, pg[:payload].byteslice(i * 4, 4).unpack1("N")) }
end

# Rewrite an index leaf (a v24 leaf with no value columns) to hold exactly `keys`.
def integrity_write_index_leaf(image, ps, page, keys)
  ends = []
  keys.reduce(0) { |acc, k| ends << acc + k.bytesize; acc + k.bytesize }
  payload = ends.pack("N*") + keys.join.b
  payload << u32(payload.bytesize + 4) # the column directory's one entry: where its (empty) regions start
  raise "index leaf overflows the page" if payload.bytesize > ps - PAGE_HEADER

  image[page * ps + PAGE_HEADER, ps - PAGE_HEADER] = payload + "\x00".b * (ps - PAGE_HEADER - payload.bytesize)
  write_page(image, ps, page, PAGE_LEAF, keys.size, 0, "".b)
end

def integrity_fixture_image(fx)
  ps = 256
  image = build_image([], [], [fx[:table] || INTEGRITY_TABLE], ps)
  roots = integrity_roots(image, ps)
  table_leaves = integrity_leaves(image, ps, roots["t"])
  index_leaves = integrity_leaves(image, ps, roots["t_v_idx"])
  raise "the table needs an interior root over several leaves" unless table_leaves.size >= 3 && index_leaves.size >= 2

  case fx[:damage]
  when nil, :none
    nil
  when :leaf_checksum
    # The last byte of t's second leaf flips; its checksum no longer matches.
    off = table_leaves[1] * ps + ps - 1
    image.setbyte(off, image.getbyte(off) ^ 0x01)
  when :leaf_keys_swapped
    # t's second leaf stores its first two rows under each other's keys.
    off = table_leaves[1] * ps + PAGE_HEADER
    n = page_u32(image, ps, table_leaves[1], 4)
    blob = off + 4 * n
    first = image.byteslice(blob, 4)
    image[blob, 4] = image.byteslice(blob + 4, 4)
    image[blob + 4, 4] = first
    refresh_body_crc(image, ps, table_leaves[1])
  when :row_rekeyed
    # The second row of t's second leaf moves into the gap above its id (an odd id: still in order).
    off = table_leaves[1] * ps + PAGE_HEADER
    n = page_u32(image, ps, table_leaves[1], 4)
    at = off + 4 * n + 4
    id = decode_int(4, image.byteslice(at, 4))
    image[at, 4] = encode_int(4, id + 1)
    refresh_body_crc(image, ps, table_leaves[1])
  when :index_entry_dropped
    # The index's second leaf loses its second entry.
    pg = read_page(image, ps, index_leaves[1])
    keys, = parse_pax_leaf(pg[:payload], pg[:item_count], [])
    integrity_write_index_leaf(image, ps, index_leaves[1], keys.dup.tap { |k| k.delete_at(1) })
  when :index_entry_extra
    # The index's last leaf gains an entry no row derives: v = 1000 (above every v) for id 1000.
    pg = read_page(image, ps, index_leaves.last)
    keys, = parse_pax_leaf(pg[:payload], pg[:item_count], [])
    extra = "\x00".b + key_body("i32", 1000) + key_body("i32", 1000)
    integrity_write_index_leaf(image, ps, index_leaves.last, keys + [extra])
  when :index_keys_swapped
    # The index's second leaf stores its first two entries in the wrong order: under full, its
    # agreement is skipped (index_unreadable) rather than probed.
    pg = read_page(image, ps, index_leaves[1])
    keys, = parse_pax_leaf(pg[:payload], pg[:item_count], [])
    keys[0], keys[1] = keys[1], keys[0]
    integrity_write_index_leaf(image, ps, index_leaves[1], keys)
  when :live_count_wrong
    # The selected meta records one live page fewer than the catalog root reaches.
    slot = select_meta(image, ps)[:slot]
    image[slot * ps + 56, 4] = u32(page_u32(image, ps, slot, 56) - 1)
    refresh_meta_crc(image, ps, slot)
  when :free_page_live
    # A new free-list page at the end of the file names t's second leaf.
    slot = select_meta(image, ps)[:slot]
    index = image.bytesize / ps
    image << "\x00".b * ps
    write_page(image, ps, index, 7, 1, 0, u32(table_leaves[1]))
    image[slot * ps + 24, 4] = u32(index + 1)
    image[slot * ps + 28, 4] = u32(index)
    refresh_meta_crc(image, ps, slot)
  when :tree_shares_page
    # t's root names the index's first leaf as its second child, in place of its own second leaf.
    set_page_u32(image, ps, roots["t"], PAGE_HEADER + 4, index_leaves[0])
  else
    raise "unknown integrity damage #{fx[:damage]}"
  end
  image
end

def integrity_expected(fx, image)
  runs = (fx[:runs] || INTEGRITY_DEFAULT_RUNS).map do |run|
    report = IntegrityCheck.new(image, level: run[:level], max_findings: run[:max_findings]).run
    { **run, report: integrity_contract(report) }
  end
  "#{JSON.pretty_generate({ fixture: "#{fx[:name]}.jed", runs: runs })}\n"
end

def generate_integrity_fixtures
  Dir.mkdir(INTEGRITY_DIR) unless Dir.exist?(INTEGRITY_DIR)
  INTEGRITY_FIXTURES.each do |fx|
    image = integrity_fixture_image(fx)
    File.binwrite(File.join(INTEGRITY_DIR, "#{fx[:name]}.jed"), image)
    File.write(File.join(INTEGRITY_DIR, "#{fx[:name]}.json"), integrity_expected(fx, image))
  end
  puts "wrote #{INTEGRITY_FIXTURES.size} integrity fixtures"
end

# The damaged fixtures are byte-identical to their generator and report what their .json records;
# every clean golden checks with no findings at either level.
# The registry (spec/integrity/findings.toml) is well-formed, and every finding the fixtures expect has a
# registered kind setting exactly the fields its entry names (`key?`: optional).
def verify_integrity_registry(reports)
  require "bundler/setup"
  require "toml-rb"
  registry = TomlRB.load_file(INTEGRITY_REGISTRY)
  checks = %w[generation trees space agreement constraints]
  kinds = {}
  registry.fetch("finding").each do |f|
    fail!("findings.toml: duplicate kind #{f["kind"]}") if kinds.key?(f["kind"])
    fail!("findings.toml: #{f["kind"]}: unknown check #{f["check"]}") unless checks.include?(f["check"])
    fail!("findings.toml: #{f["kind"]}: bad fields") unless (f["fields"] - %w[object page key key?]).empty?
    fail!("findings.toml: #{f["kind"]}: fresh must be a boolean") unless [true, false].include?(f["fresh"])
    kinds[f["kind"]] = f
  end
  reasons = registry.fetch("skip").map { |s| s["reason"] }
  fail!("findings.toml: duplicate skip reason") unless reasons.uniq == reasons
  reports.each do |name, report|
    report[:findings].each do |f|
      entry = kinds[f[:kind]] or fail!("integrity/#{name}: unregistered kind #{f[:kind]}")
      fail!("integrity/#{name}: #{f[:kind]} is not a fresh-open finding") unless entry["fresh"]
      %w[object page key].each do |field|
        set = !f[field.to_sym].nil?
        next if entry["fields"].include?("#{field}?")
        fail!("integrity/#{name}: #{f[:kind]} #{set ? "sets" : "lacks"} #{field}") unless set == entry["fields"].include?(field)
      end
    end
    report[:skipped].each do |s|
      fail!("integrity/#{name}: unregistered skip reason #{s[:reason]}") unless reasons.include?(s[:reason])
    end
  end
end

def verify_integrity(goldens)
  reports = []
  INTEGRITY_FIXTURES.each do |fx|
    image = integrity_fixture_image(fx)
    path = File.join(INTEGRITY_DIR, "#{fx[:name]}.jed")
    fail!("integrity/#{fx[:name]}.jed: missing or stale (regenerate)") unless File.exist?(path) && File.binread(path) == image
    json = File.join(INTEGRITY_DIR, "#{fx[:name]}.json")
    fail!("integrity/#{fx[:name]}.json: missing or stale (regenerate)") unless File.exist?(json) && File.read(json) == integrity_expected(fx, image)
    (fx[:runs] || INTEGRITY_DEFAULT_RUNS).each do |run|
      reports << [fx[:name], IntegrityCheck.new(image, level: run[:level], max_findings: run[:max_findings]).run]
    end
  end
  verify_integrity_registry(reports)
  goldens.each do |fx|
    image = File.binread(File.join(fixtures_dir, fx[:file]))
    %w[quick full].each do |level|
      report = IntegrityCheck.new(image, level: level, max_findings: 0).run
      fail!("#{fx[:file]}: #{level} integrity check reports #{report[:findings].first}") unless report[:findings].empty?
    end
  end
end
