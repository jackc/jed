# frozen_string_literal: true

module JedNative
  module GinRuns
    def gin_decode_run(term, run)
      length = run.byteslice(-2, 2)&.unpack1('n')
      raise Error.new('XX001', 'invalid GIN posting run') unless length && run.bytesize >= term.bytesize + length + 3
      body_start = run.bytesize - length - 2
      last = run.byteslice(term.bytesize, body_start - term.bytesize)
      body, offset, previous, keys = run.byteslice(body_start, length), 0, last, []
      while offset < length
        shared, offset = read_uvarint(body, offset)
        suffix, offset = read_uvarint(body, offset)
        raise Error.new('XX001', 'invalid GIN posting prefix') if shared > previous.bytesize || offset + suffix > length
        key = previous.byteslice(0, shared) + body.byteslice(offset, suffix)
        offset += suffix
        keys << key
        previous = key
      end
      keys << last
      raise Error.new('XX001', 'GIN postings are not sorted') unless keys.each_cons(2).all? { |a, b| a < b }
      keys
    end

    def gin_split_runs(term, keys)
      run = gin_encode_run(term, keys)
      return [run] if keys.length == 1 || run.bytesize <= FileFormat::GIN_RUN_MAX_BYTES
      middle = (keys.length + 1) / 2
      gin_split_runs(term, keys.take(middle)) + gin_split_runs(term, keys.drop(middle))
    end

    def gin_edit_posting(tree, runs, term, key, insert, capacity)
      position = runs.bsearch_index { |run| run >= term + key }
      old = position && runs[position]
      if old
        keys = gin_decode_run(term, old)
        slot = keys.bsearch_index { |item| item >= key } || keys.length
        return tree if (keys[slot] == key) == insert
        insert ? keys.insert(slot, key) : keys.delete_at(slot)
        replacements = keys.empty? ? [] : gin_split_runs(term, keys)
      elsif insert
        position = runs.length
        if runs.last
          keys = gin_decode_run(term, runs.last) + [key]
          grown = gin_encode_run(term, keys)
          if grown.bytesize <= FileFormat::GIN_RUN_MAX_BYTES
            old = runs.last
            position -= 1
            replacements = [grown]
          end
        end
        replacements ||= [gin_encode_run(term, [key])]
      else
        return tree
      end
      tree = delete_tree_key(tree, old, capacity) if old
      runs[position, old ? 1 : 0] = replacements
      replacements.each do |encoded|
        record = { key: encoded, row: [], table: { columns: [] }, forms: [], comps: [], size: encoded.bytesize }
        if tree
          result = put_tree_key(tree, encoded, record, capacity)
          tree = result[0] == :split ? { keys: [result[2]], recs: [], children: [result[1], result[3]] } : result[1]
        else
          tree = { keys: [encoded], recs: [record], children: [] }
        end
      end
      tree
    end
  end
  FileFormat.include(GinRuns)
end
