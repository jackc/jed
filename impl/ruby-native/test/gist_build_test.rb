# frozen_string_literal: true

require_relative '../lib/jed_native'

# Subtree minima are an internal build accelerator. Compare complete page bytes
# against a fresh recursive minimum walk, including splits and tied bounds.
module NativeGistBuildTests
  class UncachedCodec < JedNative::FileFormat
    def gist_subtree_min_skey(node)
      if node[:leaf]
        node[:entries].map(&:last).min
      else
        node[:children].map { |_, child| gist_subtree_min_skey(child) }.min
      end
    end
  end

  def self.run
    operations = [{ kind: :range, elem: 'i32' }]
    entries = Array.new(800) do |index|
      lower = (index * 37) % 113
      bound = { lower: lower, upper: lower + 10, lower_inc: true, upper_inc: false }
      [[bound], [799 - index].pack('N')]
    end
    pages = [JedNative::FileFormat.new, UncachedCodec.new].map do |codec|
      tree = codec.gist_build(operations, entries)
      output = {}
      root = codec.gist_serialize(operations, tree, 2, output)
      [root, output]
    end
    raise 'GiST cached subtree minima changed canonical page bytes' unless pages[0] == pages[1]
    puts 'ruby-native GiST: cached subtree minima preserve deep-tree and tied-bound bytes'
  end
end

NativeGistBuildTests.run
