# frozen_string_literal: true

module JedNative
  # spec/cost/schedule.toml [reclamation], frozen with this experiment.
  class FreeListPlan
    COMPACT_MIN_PAGES = 16
    COMPACT_GROWTH = 2
    LEAK_DEN = 16
    attr_reader :free, :head, :live_at_compaction, :protected, :compacted, :manifest_ids, :remap, :free_chain, :held_orphans

    def initialize(codec, commit, page_size, previous_live, txid, slot, truncate: true, reclaim: true, pending: {}, deferred_free: [], orphans: nil, held_orphans: [], rebuild: false)
      @codec, @pages, @page_size = codec, commit.pages, page_size
      high = commit.allocator.high_water
      safe = commit.allocator.remaining
      orphan_set = (orphans || []).to_set
      orphan_set.merge(held_orphans) if pending.empty?
      # Ownership can change between batches; never list a retained old hold
      # that a foreign generation has since made live again.
      orphan_set.subtract(commit.live_pages)
      @held_orphans = pending.empty? ? [] : (held_orphans + orphan_set.select { |page| pending.key?(page) }).uniq
      listed = (safe + deferred_free + orphan_set.reject { |page| pending.key?(page) }).uniq.sort
      compact = reclaim && rebuild
      @compacted = compact || !orphan_set.empty?
      candidates = if compact
        reached = commit.live_pages | @pages.pages.keys.to_set | pending.keys.to_set
        (codec.first_body...high).reject { |page| reached.include?(page) }
      else
        listed
      end
      @live_at_compaction = compact ? commit.live_pages.length : previous_live
      if truncate
        safe_set = safe.to_set
        high -= 1 while high > codec.first_body + 1 && safe_set.include?(high - 1)
        safe = safe.take_while { |page| page < high }
        candidates = candidates.take_while { |page| page < high }
      end
      inline_capacity = codec.inline_manifest_capacity(page_size)
      chain_capacity = (page_size - 32) / 8
      free_capacity = codec.chain_capacity(codec.payload_capacity(page_size)) / 4
      plain = codec.reader(crypto: nil)
      plain.stamp_txid = txid
      codec.stamp_txid = txid
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
        @free_chain = free_ids
        # Last page first: an encrypted free-list page binds its successor's tag (encrypted.md §7).
        (free_ids.length - 1).downto(0) do |index|
          page = free_ids[index]
          entries = persisted.slice(index * free_capacity, free_capacity) || []
          @codec.write_page(@pages, page_size, page, 7, entries.length, free_ids[index + 1] || 0, entries.pack('N*'))
        end
        references = pending.merge(@pages.pages.to_h { |page, bytes| [page, @codec.listed_crc(bytes)] }).sort
        digest = 0
        manifest_ids.each_with_index do |page, index|
          entries = references.slice(inline_capacity + index * chain_capacity, chain_capacity) || []
          payload = [txid, index, 0].pack('Q>NN') + entries.flatten.pack('NN' * entries.length)
          plain.write_page(@pages, page_size, page, 8, entries.length, manifest_ids[index + 1] || 0, payload)
          digest = @codec.crc32([page].pack('N') + @pages.pages.fetch(page), digest)
        end
        @codec.write_meta(@pages, page_size, slot, page_size, txid, commit.root, @pages.page_count, commit.live, seal: false)
        offset = slot * page_size
        @codec.write_meta_free_head(@pages, page_size, slot, @head)
        @pages[offset + 36, 12] = [manifest_ids.first || 0, references.length, manifest_ids.length].pack('N3')
        @pages[offset + 48, 8] = [digest].pack('Q>')
        inline = references.first(inline_capacity)
        @pages[offset + @codec.manifest_offset, inline.length * 8] = inline.flatten.pack('NN' * inline.length)
        @remap = commit.remap
        @codec.write_remap(@pages, page_size, slot, @remap)
        @codec.finish_meta(@pages, page_size, slot, manifest_ids.map { |page| [page, @pages.pages.fetch(page)] })
        @protected = references.map(&:first) + manifest_ids
        @manifest_ids = manifest_ids
        break
      end
    end
  end
end
