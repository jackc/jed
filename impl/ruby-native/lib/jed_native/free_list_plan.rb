# frozen_string_literal: true

module JedNative
  # spec/cost/schedule.toml [reclamation], frozen with this experiment.
  class FreeListPlan
    COMPACT_MIN_PAGES = 16
    COMPACT_GROWTH = 2
    attr_reader :free, :head, :live_at_compaction, :protected, :compacted

    def initialize(codec, commit, page_size, previous_live, txid, slot, truncate: true, reclaim: true, pending: {}, deferred_free: [])
      @codec, @pages, @page_size = codec, commit.pages, page_size
      high = commit.allocator.high_water
      safe = commit.allocator.remaining
      unlisted = high - safe.length
      compact = reclaim && (previous_live.zero? || unlisted > COMPACT_MIN_PAGES && unlisted > COMPACT_GROWTH * previous_live)
      @compacted = compact
      candidates = if compact
        reached = commit.live_pages | @pages.pages.keys.to_set | pending.keys.to_set
        (2...high).reject { |page| reached.include?(page) }
      else
        (safe + deferred_free).sort
      end
      @live_at_compaction = compact ? commit.live_pages.length : previous_live
      if truncate
        safe_set = safe.to_set
        high -= 1 while high > 3 && safe_set.include?(high - 1)
        safe = safe.take_while { |page| page < high }
        candidates = candidates.take_while { |page| page < high }
      end
      inline_capacity = (page_size - 64) / 12
      chain_capacity = (page_size - 32) / 12
      free_capacity = (page_size - 16) / 4
      body_count = pending.length + @pages.pages.length
      manifest_count = 0
      loop do
        allocation = PageAllocator.new(high, safe)
        manifest_ids = Array.new(manifest_count) { allocation.allocate }
        persisted = candidates - manifest_ids
        free_ids = []
        if persisted.length >= 2
          while (persisted.length + free_capacity - 1) / free_capacity > free_ids.length
            id = allocation.allocate
            free_ids << id
            persisted.delete(id)
          end
        end
        required = ([body_count + free_ids.length - inline_capacity, 0].max + chain_capacity - 1) / chain_capacity
        if required > manifest_count
          manifest_count = required
          next
        end
        @pages.resize(allocation.high_water)
        @free, @head = persisted, free_ids.first || 0
        free_ids.each_with_index do |page, index|
          entries = persisted.slice(index * free_capacity, free_capacity) || []
          @codec.write_page(@pages, page_size, page, 7, entries.length, free_ids[index + 1] || 0, entries.pack('N*'))
        end
        references = pending.merge(@pages.pages.to_h { |page, bytes| [page, @codec.crc64(bytes)] }).sort
        digest = 0
        manifest_ids.each_with_index do |page, index|
          entries = references.slice(inline_capacity + index * chain_capacity, chain_capacity) || []
          payload = [txid, index, 0].pack('Q>NN') + entries.flatten.pack('NQ>' * entries.length)
          @codec.write_page(@pages, page_size, page, 8, entries.length, manifest_ids[index + 1] || 0, payload)
          digest = @codec.crc64([page].pack('N') + @pages.pages.fetch(page), digest)
        end
        @codec.write_meta(@pages, page_size, slot, page_size, txid, commit.root, @pages.page_count, commit.live)
        offset = slot * page_size
        @pages[offset + 28, 4] = [@head].pack('N')
        @pages[offset + 36, 12] = [manifest_ids.first || 0, references.length, manifest_ids.length].pack('N3')
        @pages[offset + 48, 8] = [digest].pack('Q>')
        inline = references.first(inline_capacity)
        @pages[offset + 64, inline.length * 12] = inline.flatten.pack('NQ>' * inline.length)
        @pages[offset + 32, 4] = [@codec.meta_crc(@pages.pages.fetch(slot))].pack('N')
        @protected = references.map(&:first) + manifest_ids
        break
      end
    end
  end
end
