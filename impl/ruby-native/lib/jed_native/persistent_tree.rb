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
        if recs_payload_size([record]) > capacity
          raise Error.new('0A000', 'record exceeds page capacity after spilling')
        end
        if root
          result = put_tree_key(root, key, record, capacity)
          root = grow_root(result, capacity)
        else
          root = { keys: [key], recs: [record], children: [] }
        end
      end
      root
    end

    def same_tree_record?(left, right)
      left[:row] == right[:row] && left[:forms] == right[:forms] && left[:comps] == right[:comps] &&
        leaf_table_cols(left[:table]) == leaf_table_cols(right[:table]) &&
        tree_fixed_key?(left[:table]) == tree_fixed_key?(right[:table])
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
        index = node[:keys].bsearch_index { |item| (item <=> key) >= 0 } || node[:keys].length
        keys, records = node[:keys].dup, node[:recs].dup
        if keys[index] == key
          records[index] = record
        else
          keys.insert(index, key)
          records.insert(index, record)
        end
        return split_node({ keys: keys, recs: records, children: [] }, capacity, index)
      end
      index = node[:keys].bsearch_index { |item| (item <=> key) > 0 } || node[:keys].length
      result = put_tree_key(node[:children][index], key, record, capacity)
      children = node[:children].dup
      if result[0] == :split
        children[index, 1] = result[1]
        keys = node[:keys].dup.insert(index, *result[2])
        split_node({ keys: keys, recs: [], children: children }, capacity, index + result[2].length - 1)
      else
        children[index] = result[1]
        [:whole, { keys: node[:keys], recs: [], children: children }]
      end
    end

    def empty_unary_tree?(node)
      node = node[:children][0] while node[:keys].empty? && node[:children].length == 1
      node[:keys].empty? && node[:children].empty?
    end

    def delete_tree_key(root, key, capacity)
      return nil unless root
      node = delete_tree_path(root, key, capacity)
      node = node[:children].first while !node[:children].empty? && node[:keys].empty?
      node[:children].empty? && node[:keys].empty? ? nil : node
    end

    def delete_tree_path(node, key, capacity)
      if node[:children].empty?
        index = node[:keys].bsearch_index { |item| (item <=> key) >= 0 }
        return node unless index && node[:keys][index] == key
        keys, records = node[:keys].dup, node[:recs].dup
        keys.delete_at(index)
        records.delete_at(index)
        return { keys: keys, recs: records, children: [] }
      end
      index = node[:keys].bsearch_index { |item| (item <=> key) > 0 } || node[:keys].length
      child = delete_tree_path(node[:children][index], key, capacity)
      return node if child.equal?(node[:children][index])
      children = node[:children].dup
      children[index] = child
      keys = node[:keys].dup
      if children.length > 1 && !child[:children].empty? && empty_unary_tree?(child)
        children.delete_at(index)
        keys.delete_at([index, keys.length - 1].min)
        return { keys: keys, recs: [], children: children }
      end
      if children.length > 1 && 2 * node_payload(child, capacity) < capacity
        left_index = index + 1 < children.length ? index : index - 1
        left, right = children[left_index, 2]
        merged = if left[:children].empty?
          { keys: left[:keys] + right[:keys], recs: left[:recs] + right[:recs], children: [] }
        else
          { keys: left[:keys] + [keys[left_index]] + right[:keys], recs: [], children: left[:children] + right[:children] }
        end
        begin
          result = split_node(merged, capacity, nil)
          if result[0] == :whole
            children[left_index, 2] = [result[1]]
            keys.delete_at(left_index)
          elsif result[1].length == 2 &&
              node_payload(node, capacity) - keys[left_index].bytesize + result[2][0].bytesize <= capacity
            children[left_index, 2] = result[1]
            keys[left_index] = result[2][0]
          end
        rescue Error => error
          # A merge never grows the parent or produces more than two children.
          raise unless !merged[:children].empty? && error.sqlstate == '0A000'
        end
      end
      { keys: keys, recs: [], children: children }
    end
  end
end
