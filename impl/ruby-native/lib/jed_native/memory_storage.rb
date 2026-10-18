# frozen_string_literal: true

require_relative 'commit_plan'
require_relative 'free_list_plan'

module JedNative
  # The in-memory host stores the same packed pages. Its free list stays in RAM,
  # so it emits no free-list or manifest pages and has no durability barrier.
  class MemoryStorage
    attr_reader :page_size, :page_count, :state

    def initialize(codec, page_size: 8192, image: nil, state: nil)
      @codec, @page_size = codec, page_size
      image ||= codec.build_image([], [], [], page_size)
      @pages = {}
      @page_count = image.bytesize / page_size
      @page_count.times { |page| @pages[page] = image.byteslice(page * page_size, page_size) }
      @free = state ? state.fetch(:free_pages, []) : []
      @live_pages = state ? state.fetch(:live_pages).to_set : Set[2]
      @last_written = []
      @live_at_compaction = @page_count - @free.length
      @catalog_pages = 1
      @page_ids, @overflow_ids = {}.compare_by_identity, {}.compare_by_identity
      @state = state || { tables: [], types: [], sequences: [], views: [], collations: [] }
    end

    def prepare(state, limit: 0, stages_rows: true)
      plan = build(state)
      unless fits?(plan, limit)
        reached = @live_pages | @last_written.to_set
        free = (2...@page_count).reject { |page| reached.include?(page) }
        if free.length > @free.length
          @free = free
          @live_at_compaction = reached.length
          plan = build(state)
        end
        catalog = plan.pages.pages.count { |_, bytes| bytes.getbyte(0) == FileFormat::PAGE_CATALOG }
        unless fits?(plan, limit) || !stages_rows && catalog <= @catalog_pages
          raise Error.new('54P06', 'database storage limit exceeded')
        end
      end
      plan
    end

    def adopt(plan, state)
      @pages.merge!(plan.pages.pages)
      @page_count, @free = plan.pages.page_count, plan.allocator.remaining
      @live_pages, @last_written = plan.live_pages, plan.pages.pages.keys
      @page_ids, @overflow_ids = plan.adopt, plan.overflow_ids
      @catalog_pages = plan.pages.pages.count { |_, bytes| bytes.getbyte(0) == FileFormat::PAGE_CATALOG }
      @state = state
      unlisted = @page_count - @free.length
      if unlisted > FreeListPlan::COMPACT_MIN_PAGES && unlisted > FreeListPlan::COMPACT_GROWTH * @live_at_compaction
        reached = @live_pages | @last_written.to_set
        @free = (2...@page_count).reject { |page| reached.include?(page) }
        @live_at_compaction = reached.length
      end
    end

    private

    def build(state)
      CommitPlan.new(@codec, state, @page_size, PageAllocator.new(@page_count, @free), page_ids: @page_ids, overflow_ids: @overflow_ids)
    end

    def fits?(plan, limit)
      limit <= 0 || plan.pages.page_count <= @page_count || plan.pages.page_count * @page_size <= limit
    end
  end
end
