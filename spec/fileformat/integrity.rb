# The integrity-check reference (spec/design/integrity-check.md): an independent fourth voice for the
# report every core's `integrity_check` returns. Loaded by verify.rb, whose decoder helpers it reuses
# (meta selection, page reads, leaf and value decoding, key encoding), and run as
#
#   ruby spec/fileformat/verify.rb --integrity-check [--quick] [--max-findings N] FILE…
#
# It checks a file as a freshly opened handle would see it: meta selection, the catalog, and the free
# list's chain must load (as `open` requires), and everything after that is a finding, not an error.
# It covers the checks it can evaluate without SQL. Index agreement for an expression or partial index
# whose text falls outside the subset expr.rb evaluates is listed under `reference_unchecked` (not part
# of the cross-core report), as are constraints until slice I4.

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
    entries = []
    if ix[:kind] == "gist"
      walk = GistWalk.new(self, ix[:name], gist_opclasses_for(t, ix))
      readable = walk.run(ix[:root_page])
      entries = walk.entries
      count = entries.size
    else
      walk = BtreeWalk.new(self, ix[:name], nil, gin: ix[:kind] == "gin")
      readable = walk.run(ix[:root_page]) { |_page, key, _values| entries << key }
      count = ix[:kind] == "gin" ? walk.postings : entries.size
    end
    @damaged ||= !readable
    return unless @level == "full"

    return skip(ix[:name], "table_unreadable") if rows.nil?
    return skip(ix[:name], "index_unreadable") unless readable && @findings.size == before

    asts = begin
      [ix[:predicate] && StoredExpr.parse(ix[:predicate])] +
        ix[:cols].map { |c| c.is_a?(Hash) ? StoredExpr.parse(c[:expr]) : nil }
    rescue StoredExpr::Unsupported
      return @unchecked << { object: ix[:name], check: "index_agreement" }
    end

    # Derive every row's entries before reporting, so an expression outside the subset leaves the
    # index unchecked rather than half-reported.
    derived = begin
      rows.map do |key, row|
        [key, derive_entries(t, ix, asts[0], key, row)]
      rescue StoredExpr::EvalError => e
        [key, e]
      end
    rescue StoredExpr::Unsupported
      return @unchecked << { object: ix[:name], check: "index_agreement" }
    end
    found = 0
    derived.each do |key, want|
      if want.is_a?(StoredExpr::EvalError)
        add("index_entry_error", object: ix[:name], key: key, detail: "#{want.code}: #{want.message}")
        next
      end
      want.each do |entry|
        if index_holds?(ix, entries, entry)
          found += 1
        else
          add("index_entry_missing", object: ix[:name], key: key, detail: "no entry for the row")
        end
      end
    end
    return if count == found

    add("index_entry_count_mismatch", object: ix[:name],
                                      detail: "index holds #{count} entries, #{found} derive from rows")
  end

  # The entries one row derives, in entry order (integrity-check.md §3.4): a B-tree's entry key, a GIN
  # index's [term, storage key] postings in term order, a GiST index's [bound, storage key] leaf entry.
  def derive_entries(t, ix, predicate, key, row)
    case ix[:kind]
    when "gin"
      terms = []
      ix[:cols].each_with_index do |ci, pos|
        v = row[ci]
        next if v.nil?

        tag = ix[:cols].size > 1 ? [pos].pack("C") : "".b
        type = t[:columns][ci][:type]
        col_terms = if type == "jsonb"
                      gin_jsonb_terms(v)
                    else
                      (v.is_a?(Hash) ? v[:elements] : v).compact.map { |e| gin_element_term(array_elem(type), e) }
                    end
        col_terms.map(&:b).uniq.each { |term| terms << (tag + term).b }
      end
      terms.sort.map { |term| [term, key] }
    when "gist"
      # The predicate first, as the write path evaluates it, then the NULL rule (gist.md §4.1).
      return [] if predicate && !StoredExpr.filter?(predicate, t[:columns], row)
      return [] if ix[:cols].any? { |ci| row[ci].nil? }

      ops = gist_opclasses_for(t, ix)
      [[gist_encode_bound(ops, gist_leaf_bound(ops, row, ix[:cols])).b, key]]
    else
      return [] if predicate && !StoredExpr.filter?(predicate, t[:columns], row)

      [index_entry_key(t, ix, key, row)]
    end
  end

  # Whether the index holds a derived entry, found as the core's lookup finds it: a B-tree or GiST
  # entry by equality; a GIN posting in the first run at or after `term ‖ k`, decoded with the term's
  # length (an undecodable run holds nothing).
  def index_holds?(ix, entries, entry)
    case ix[:kind]
    when "gin"
      term, k = entry
      run = entries.bsearch { |r| r >= term + k }
      return false unless run&.start_with?(term)

      begin
        gin_decode_run(term.bytesize, run).include?(k)
      rescue StandardError
        false
      end
    when "gist" then entries.include?(entry)
    else
      @entry_sets ||= {}
      (@entry_sets[ix[:name]] ||= entries.to_set).include?(entry)
    end
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
    def initialize(check, object, columns, gin: false)
      @check = check
      @object = object
      @columns = columns
      @gin = gin
      @leaf_depth = nil
      @readable = true
      @postings = 0
    end

    # A GIN tree's postings, counted from its runs' tails (integrity-check.md §3.2).
    attr_reader :postings

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
        if @gin
          begin
            @postings += keys.sum { |key| gin_run_postings(key) }
          rescue StandardError => e
            return corrupt(page, e.message)
          end
        end
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

  # Walk a GiST R-tree (gist.md §4.1): page verification, node types, uniform leaf depth, every bound
  # decoding under the index's opclasses, and each interior bound containing every bound in its child
  # (integrity-check.md §3.2). Collects the leaf entries as [bound bytes, storage key].
  class GistWalk
    attr_reader :entries

    def initialize(check, object, ops)
      @check = check
      @object = object
      @ops = ops
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
      if depth >= INTEGRITY_MAX_TREE_DEPTH
        @readable = false
        return @check.finding("page_corrupt", object: @object, page: page, detail: "deeper than any valid tree")
      end
      begin
        pg = read_page(@check.image, @check.ps, page)
        items = parse(pg)
      rescue StandardError => e
        @readable = false
        return @check.finding("page_corrupt", object: @object, page: page, detail: e.message)
      end
      if items.nil?
        @readable = false
        return @check.finding("page_type_wrong", object: @object, page: page,
                                                 detail: "page type #{pg[:type]} in a GiST tree")
      end
      leaf = pg[:type] == PAGE_GIST_LEAF
      if leaf
        @leaf_depth ||= depth
        if depth != @leaf_depth
          @check.finding("tree_depth_uneven", object: @object, page: page,
                                              detail: "leaf at depth #{depth}, others at #{@leaf_depth}")
        end
      end
      if parent_bound && !items.all? { |bound, _, _| gist_bound_contains?(@ops, parent_bound, bound) }
        @check.finding("gist_bounds_violation", object: @object, page: page,
                                                detail: "a bound lies outside its parent's bound for this page")
      end
      if leaf
        items.each { |_, raw, skey| @entries << [raw, skey] }
      else
        items.each { |bound, _, child| visit(child, depth + 1, bound) }
      end
    end

    # [decoded bound, bound bytes, storage key or child page] per entry; nil for another page type.
    def parse(pg)
      payload = pg[:payload]
      pos = 0
      return nil unless [PAGE_GIST_LEAF, PAGE_GIST_INTERIOR].include?(pg[:type])

      (0...pg[:item_count]).map do
        bl, pos = take(payload, pos, 2)
        raw, pos = take(payload, pos, bl.unpack1("n"))
        bound = gist_decode_bound(@ops, raw.b)
        if pg[:type] == PAGE_GIST_LEAF
          sl, pos = take(payload, pos, 2)
          skey, pos = take(payload, pos, sl.unpack1("n"))
          [bound, raw.b, skey.b]
        else
          cp, pos = read_ptr(payload, pos)
          [bound, raw.b, cp]
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

# The I3 table: an expression index, a partial index, a GIN over an i32[] and a GiST over an i32range,
# each several pages deep at a 256-byte page (indexes in name order). Some rows have a NULL or empty
# array, and some a NULL range, which derive no GIN postings or GiST entry.
INTEGRITY_U_TABLE = {
  name: "u",
  columns: [col("id", "i32", pk: true), col("v", "i32"), col("tags", "i32[]"), col("span", "i32range")],
  indexes: [{ name: "u_span_gist", cols: [3], kind: "gist" },
            { name: "u_tags_gin", cols: [2], kind: "gin" },
            { name: "u_v1_idx", cols: [{ expr: "v + 1" }] },
            { name: "u_vp_idx", cols: [1], predicate: "v < 50" }],
  rows: (1..40).map do |i|
    tags = if (i % 11).zero? then nil
           elsif (i % 13).zero? then []
           else [i % 5, i % 7, i % 3]
           end
    [i * 2, (i * 37) % 101, tags, (i % 9).zero? ? nil : rng(i, i + 2 + (i % 4))]
  end
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
  { name: "tree_shares_page", damage: :tree_shares_page },
  { name: "u_clean", damage: nil, table: INTEGRITY_U_TABLE },
  { name: "gin_posting_dropped", damage: :gin_posting_dropped, table: INTEGRITY_U_TABLE },
  { name: "gin_posting_extra", damage: :gin_posting_extra, table: INTEGRITY_U_TABLE },
  { name: "gin_run_malformed", damage: :gin_run_malformed, table: INTEGRITY_U_TABLE },
  { name: "gist_entry_dropped", damage: :gist_entry_dropped, table: INTEGRITY_U_TABLE },
  { name: "gist_bound_shrunk", damage: :gist_bound_shrunk, table: INTEGRITY_U_TABLE },
  { name: "expr_entry_wrong", damage: :expr_entry_wrong, table: INTEGRITY_U_TABLE },
  { name: "expr_overflow", damage: :expr_overflow, table: INTEGRITY_U_TABLE },
  { name: "partial_entry_extra", damage: :partial_entry_extra, table: INTEGRITY_U_TABLE }
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
  return integrity_u_damage(image, ps, roots, fx[:damage]) if roots.key?("u")

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

# The leaf pages of a plaintext GiST tree in child order, as [page, entries] with each entry the raw
# [bound, storage key] pair.
def integrity_gist_leaves(image, ps, root)
  pg = read_page(image, ps, root)
  pos = 0
  items = (0...pg[:item_count]).map do
    bl, pos = take(pg[:payload], pos, 2)
    bound, pos = take(pg[:payload], pos, bl.unpack1("n"))
    if pg[:type] == PAGE_GIST_LEAF
      sl, pos = take(pg[:payload], pos, 2)
      skey, pos = take(pg[:payload], pos, sl.unpack1("n"))
      [bound.b, skey.b]
    else
      cp, pos = take(pg[:payload], pos, 4)
      [bound.b, cp.unpack1("N")]
    end
  end
  return [[root, items]] if pg[:type] == PAGE_GIST_LEAF

  items.flat_map { |_, child| integrity_gist_leaves(image, ps, child) }
end

# The keys of a plaintext index leaf.
def integrity_index_keys(image, ps, page)
  pg = read_page(image, ps, page)
  parse_pax_leaf(pg[:payload], pg[:item_count], [])[0].map(&:b)
end

def integrity_u_damage(image, ps, roots, damage)
  table_leaves = integrity_leaves(image, ps, roots["u"])
  gin_leaves = integrity_leaves(image, ps, roots["u_tags_gin"])
  gist_root = read_page(image, ps, roots["u_span_gist"])
  raise "u's trees need several pages" unless table_leaves.size >= 3 && gin_leaves.size >= 2 &&
                                              gist_root[:type] == PAGE_GIST_INTERIOR
  case damage
  when nil
    nil
  when :gin_posting_dropped
    # The first run of three or more storage keys in the GIN's first leaf loses its second key.
    keys = integrity_index_keys(image, ps, gin_leaves[0])
    at = keys.index { |run| gin_decode_run(4, run).size >= 3 }
    postings = gin_decode_run(4, keys[at])
    postings.delete_at(1)
    keys[at] = gin_encode_run(keys[at].byteslice(0, 4), postings)
    integrity_write_index_leaf(image, ps, gin_leaves[0], keys)
  when :gin_posting_extra
    # The first run of two or more keys in the GIN's second leaf gains an odd id no row has.
    keys = integrity_index_keys(image, ps, gin_leaves[1])
    at = (1...keys.size).find { |i| gin_decode_run(4, keys[i]).size >= 2 } # not the leaf's first: its key is a separator
    postings = gin_decode_run(4, keys[at])
    postings.insert(1, key_body("i32", decode_int(4, postings[0]) + 1))
    keys[at] = gin_encode_run(keys[at].byteslice(0, 4), postings)
    integrity_write_index_leaf(image, ps, gin_leaves[1], keys)
  when :gin_run_malformed
    # The GIN's first leaf's second run records a body longer than the run itself.
    keys = integrity_index_keys(image, ps, gin_leaves[0])
    keys[1] = keys[1].byteslice(0, keys[1].bytesize - 2) + u16(0xFFFF)
    integrity_write_index_leaf(image, ps, gin_leaves[0], keys)
  when :gist_entry_dropped
    # The GiST's second leaf loses its second entry; every bound above it still contains the rest.
    page, items = integrity_gist_leaves(image, ps, roots["u_span_gist"])[1]
    items.delete_at(1)
    payload = items.map { |bound, skey| u16(bound.bytesize) + bound + u16(skey.bytesize) + skey }.join.b
    image[page * ps + PAGE_HEADER, ps - PAGE_HEADER] = payload + "\x00".b * (ps - PAGE_HEADER - payload.bytesize)
    write_page(image, ps, page, PAGE_GIST_LEAF, items.size, 0, "".b)
  when :gist_bound_shrunk
    # The GiST root's bound for its first child starts one past the lowest lower bound beneath it.
    # The root's first entry is u16 length, then the range body: flags, then the lower bound's i32.
    off = roots["u_span_gist"] * ps + PAGE_HEADER + 3
    image[off, 4] = encode_int(4, decode_int(4, image.byteslice(off, 4)) + 1)
    refresh_body_crc(image, ps, roots["u_span_gist"])
  when :expr_entry_wrong
    # The expression index's largest entry (v + 1 of the row with the largest v) holds v + 2.
    leaf = integrity_leaves(image, ps, roots["u_v1_idx"]).last
    keys = integrity_index_keys(image, ps, leaf)
    last = keys.last
    keys[-1] = "\x00".b + encode_int(4, decode_int(4, last.byteslice(1, 4)) + 1) + last.byteslice(5..)
    integrity_write_index_leaf(image, ps, leaf, keys)
  when :expr_overflow
    # The second row of u's second leaf holds v = 2147483647, so its v + 1 overflows.
    page = table_leaves[1]
    n = page_u32(image, ps, page, 4)
    pg = read_page(image, ps, page)
    key_end = pg[:payload].byteslice(4 * (n - 1), 4).unpack1("N")
    dir = PAGE_HEADER + 4 * n + key_end # the column directory: u32 start per column, then the end
    v_region = page_u32(image, ps, page, dir + 4)
    off = page * ps + PAGE_HEADER + v_region + 1 + (n + 7) / 8 + 4 * 1
    image[off, 4] = encode_int(4, 2**31 - 1)
    refresh_body_crc(image, ps, page)
  when :partial_entry_extra
    # The partial index's last leaf gains, at its end, the entry of row id 4 (v = 74, not < 50).
    leaf = integrity_leaves(image, ps, roots["u_vp_idx"]).last
    keys = integrity_index_keys(image, ps, leaf)
    integrity_write_index_leaf(image, ps, leaf, keys + ["\x00".b + key_body("i32", 74) + key_body("i32", 4)])
  else
    raise "unknown integrity damage #{damage}"
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

# --- GIN posting runs and GiST bounds (integrity-check.md §3.2, §3.4) -------------------------------

def gin_read_run_varint(buf, pos)
  v = 0
  [0, 7, 14].each do |shift|
    raise "a GIN posting run is malformed" if pos >= buf.bytesize

    c = buf.getbyte(pos)
    pos += 1
    v |= (c & 0x7F) << shift
    return [v, pos] if (c & 0x80).zero?
  end
  raise "a GIN posting run is malformed"
end

# A run's posting count from its tail, without the term's length: the body (its length in the last two
# bytes, after at least a term byte and a `last` byte) parses exactly as shared/suffix triples.
def gin_run_postings(run)
  n = run.bytesize
  raise "a GIN posting run is malformed" if n < 4

  body_len = run.byteslice(n - 2, 2).unpack1("n")
  raise "a GIN posting run is malformed" if n - 2 - body_len < 2

  body = run.byteslice(n - 2 - body_len, body_len)
  at = 0
  count = 1
  while at < body.bytesize
    shared, at = gin_read_run_varint(body, at)
    suffix, at = gin_read_run_varint(body, at)
    raise "a GIN posting run is malformed" if at + suffix > body.bytesize || shared + suffix == 0

    at += suffix
    count += 1
  end
  count
end

# A run's storage keys, ascending, given its term's length (gin.md §4.5; the cores' run decoder).
def gin_decode_run(term_len, run)
  n = run.bytesize
  raise "a GIN posting run is malformed" if n < term_len + 3

  body_len = run.byteslice(n - 2, 2).unpack1("n")
  last_end = n - 2 - body_len
  raise "a GIN posting run is malformed" if last_end <= term_len

  last = run.byteslice(term_len, last_end - term_len)
  body = run.byteslice(last_end, body_len)
  keys = []
  at = 0
  while at < body.bytesize
    shared, at = gin_read_run_varint(body, at)
    suffix, at = gin_read_run_varint(body, at)
    prev = keys.last || last
    raise "a GIN posting run is malformed" if shared > prev.bytesize || at + suffix > body.bytesize || shared + suffix == 0

    key = prev.byteslice(0, shared) + body.byteslice(at, suffix)
    at += suffix
    raise "a GIN posting run is malformed" if keys.last && keys.last >= key

    keys << key
  end
  raise "a GIN posting run is malformed" if keys.last && keys.last >= last

  keys << last
end

# Decode a GiST tuple bound (gist.md §4.1, §6): per opclass a range body or a `[min, max]` pair of
# length-prefixed key bytes, consuming the bytes exactly.
def gist_decode_bound(ops, raw)
  pos = 0
  bound = ops.map do |op|
    if op[:kind] == :range
      v, pos = decode_range_body(op[:elem], raw, pos)
      v
    else
      ml, pos = take(raw, pos, 2)
      min, pos = take(raw, pos, ml.unpack1("n"))
      xl, pos = take(raw, pos, 2)
      max, pos = take(raw, pos, xl.unpack1("n"))
      [min.b, max.b]
    end
  end
  raise "GiST bound has trailing bytes" unless pos == raw.bytesize

  bound
end

# Whether a decoded tuple bound contains another, per component: range `@>` (bounds compared through
# their order-preserving key encodings), scalar `[min, max]` bytewise.
def gist_bound_contains?(ops, outer, inner)
  ops.each_index.all? do |k|
    a = outer[k]
    b = inner[k]
    if ops[k][:kind] == :range
      next true if b == :empty
      next false if a == :empty

      elem = ops[k][:elem]
      bound = ->(v, side, lower) { encode_range_key_bound(+"".b, elem, v, side, lower) }
      bound.(a, :lower, true) <= bound.(b, :lower, true) && bound.(b, :upper, false) <= bound.(a, :upper, false)
    else
      a[0] <= b[0] && b[1] <= a[1]
    end
  end
end
