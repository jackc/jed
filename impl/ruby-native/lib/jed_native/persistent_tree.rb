# frozen_string_literal: true

module JedNative
  # Immutable page-sized B+tree edits. Existing nodes retain their page identity;
  # a changed path has no page until a successful commit assigns one.
  module PersistentTrees
    def patch_tree(root, pairs, capacity)
      records = {}
      visit = lambda do |node|
        next unless node
        if node[:children].empty?
          node[:keys].zip(node[:recs]) { |key, record| records[key] = record }
        else
          node[:children].each { |child| visit.call(child) }
        end
      end
      visit.call(root)
      present = pairs.to_h
      records.each_key { |key| root = delete_tree_key(root, key, capacity) unless present.key?(key) }
      pairs.each do |key, record|
        old = records[key]
        next if old && same_tree_record?(old, record)
        maximum = record_max(capacity, record[:table][:columns].length)
        raise Error.new('0A000', "record exceeds page capacity after spilling") if record[:size] > maximum
        if root
          result = put_tree_key(root, key, record, capacity)
          root = result[0] == :split ? { keys: [result[2]], recs: [], children: [result[1], result[3]] } : result[1]
        else
          root = { keys: [key], recs: [record], children: [] }
        end
      end
      root
    end

    def same_tree_record?(left, right)
      left[:row] == right[:row] && left[:forms] == right[:forms] && left[:comps] == right[:comps] &&
        left[:table][:columns].map { |column| column[:type] } == right[:table][:columns].map { |column| column[:type] }
    end

    def tree_from_profile(profile, pairs)
      records = pairs.to_h
      convert = lambda do |node|
        next nil unless node
        children = node[:children].map { |child| convert.call(child) }
        recs = if children.empty?
          node[:keys].map do |key|
            records.fetch(key) do
              # A skewed or unavailable index is retained verbatim and is not
              # seekable. Its stored keys need not match the loaded collation.
              if pairs.empty? || pairs.first[1][:table][:columns].empty?
                { key: key, row: [], table: { columns: [] }, forms: [], comps: [], size: key.bytesize }
              else
                raise KeyError, 'stored primary key has no corresponding row'
              end
            end
          end
        else
          []
        end
        { keys: node[:keys], recs: recs, children: children, page: node[:page], overflow_pages: node[:overflow_pages] }
      end
      convert.call(profile[:tree])
    end

    def put_tree_key(node, key, record, capacity)
      if node[:children].empty?
        index = node[:keys].bsearch_index { |item| item >= key } || node[:keys].length
        keys, records = node[:keys].dup, node[:recs].dup
        if keys[index] == key
          records[index] = record
        else
          keys.insert(index, key)
          records.insert(index, record)
        end
        return split_node({ keys: keys, recs: records, children: [] }, capacity, index == keys.length - 1)
      end
      index = node[:keys].bsearch_index { |item| item > key } || node[:keys].length
      result = put_tree_key(node[:children][index], key, record, capacity)
      children = node[:children].dup
      if result[0] == :split
        children[index] = result[1]
        children.insert(index + 1, result[3])
        keys = node[:keys].dup.insert(index, result[2])
        split_node({ keys: keys, recs: [], children: children }, capacity, index == keys.length - 1)
      else
        children[index] = result[1]
        [:whole, { keys: node[:keys], recs: [], children: children }]
      end
    end

    def delete_tree_key(root, key, capacity)
      return nil unless root
      node = delete_tree_path(root, key, capacity)
      node = node[:children].first while !node[:children].empty? && node[:keys].empty?
      node[:children].empty? && node[:keys].empty? ? nil : node
    end

    def delete_tree_path(node, key, capacity)
      if node[:children].empty?
        index = node[:keys].bsearch_index { |item| item >= key }
        return node unless index && node[:keys][index] == key
        keys, records = node[:keys].dup, node[:recs].dup
        keys.delete_at(index)
        records.delete_at(index)
        return { keys: keys, recs: records, children: [] }
      end
      index = node[:keys].bsearch_index { |item| item > key } || node[:keys].length
      child = delete_tree_path(node[:children][index], key, capacity)
      return node if child.equal?(node[:children][index])
      children = node[:children].dup
      children[index] = child
      keys = node[:keys].dup
      if children.length > 1 && 2 * node_payload(child) < capacity
        left_index = index + 1 < children.length ? index : index - 1
        left, right = children[left_index, 2]
        merged = if left[:children].empty?
          { keys: left[:keys] + right[:keys], recs: left[:recs] + right[:recs], children: [] }
        else
          { keys: left[:keys] + [keys[left_index]] + right[:keys], recs: [], children: left[:children] + right[:children] }
        end
        begin
          result = split_node(merged, capacity, false)
          if result[0] == :whole
            children[left_index, 2] = [result[1]]
            keys.delete_at(left_index)
          else
            children[left_index, 2] = [result[1], result[3]]
            keys[left_index] = result[2]
          end
        rescue RuntimeError => error
          # Near-capacity separators can make a merged interior unsplittable.
          # Keeping the original pair is the deterministic format contract.
          raise unless !merged[:children].empty? && error.message.start_with?('unsplittable interior')
        end
      end
      { keys: keys, recs: [], children: children }
    end
  end
end
