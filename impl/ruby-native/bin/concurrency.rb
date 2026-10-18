# frozen_string_literal: true

# The shared schedules prescribe a total order. A blocked writer is queued and
# admitted immediately after the holder ends; host tests separately use threads.
class NativeCorpus
  def run_concurrency_file(path, contents)
    if @mode == 'disk' && contents.match?(/^#\s*requires:.*resource\.storage_memory\b/)
      @summary[:skipped_files] += 1
      return
    end
    directory = @mode == 'disk' ? Dir.mktmpdir('jed-ruby-concurrency') : nil
    db = directory ? JedNative.create(File.join(directory, 'database.jed')) : JedNative.memory
    contents.scan(/^#\s*attach:\s*(.+)$/).flatten.each { |name| db.attach(name.strip) }
    contents.scan(/^#\s*max_storage_bytes:\s*(\d+)(?:\s+(\w+))?/).each { |amount, name| db.set_max_storage_bytes(amount.to_i, name || 'main') }
    handles, waiting = {}, []
    lines, i = contents.lines.map(&:chomp), 0
    control = /\A(?:open |on |commit |rollback |close |expect )/
    while i < lines.length
      line, location = lines[i].sub(/\s+#.*$/, '').strip, i + 1
      i += 1
      next if line.empty? || line.start_with?('#')
      @summary[:records] += 1
      begin
        words = line.split
        case words[0]
        when 'open'
          name, mode, blocked = words[1..3]
          if blocked == 'blocks'
            raise 'writer did not block' unless db.writer_held?
            waiting << name
          else
            handles[name] = mode == 'read' ? db.read_session : db.write_session
          end
        when 'commit', 'rollback', 'close'
          handle = handles.fetch(words[1])
          handle.public_send(words[0])
          if words[0] != 'close'
            handle.close
            handles.delete(words[1])
          end
          if !waiting.empty? && !db.writer_held?
            name = waiting.shift
            handles[name] = db.write_session
          end
        when 'expect'
          actual = words[1] == 'version' ? db.version : db.oldest_live_txid
          raise "#{words[1]} expected #{words[2]}, got #{actual}" unless actual == Integer(words[2])
        when 'on'
          handle = handles.fetch(words[1])
          sql = []
          while i < lines.length && lines[i] != '----' && !lines[i].empty? && !lines[i].match?(control)
            sql << lines[i] unless lines[i].start_with?('#')
            i += 1
          end
          expected = []
          if lines[i] == '----'
            i += 1
            while i < lines.length && !lines[i].empty? && !lines[i].match?(control)
              expected << lines[i] unless lines[i].start_with?('#')
              i += 1
            end
          end
          error = nil
          result = begin
            handle.execute(sql.join("\n"))
          rescue JedNative::Error => caught
            error = caught
            nil
          end
          if words[2] == 'statement' && words[3] == 'error'
            raise "expected #{words[4]}, got #{error ? error.sqlstate : 'success'}" unless error&.sqlstate == words[4]
          else
            raise error if error
            if words[2] == 'query'
              tags, sort = words[3..4]
              tags *= result.columns.length if tags.length == 1
              raise 'column count mismatch' unless tags.length == result.columns.length
              rows = result.rows.map { |row| row.each_with_index.map { |value, n| handle.render(value, result.column_types[n]) } }
              rows.sort! if sort == 'rowsort'
              actual = rows.flatten
              actual.sort! if sort == 'valuesort'
              expected = expected.each_slice(tags.length).to_a.sort.flatten if sort == 'rowsort'
              expected.sort! if sort == 'valuesort'
              raise "rows expected #{expected.inspect}; got #{actual.inspect}" unless actual == expected
            end
          end
        else raise "unknown concurrency command #{line}"
        end
        @summary[:passed] += 1
      rescue StandardError => error
        raise if error.is_a?(Timeout::Error)
        fail_record(path, location, "#{error.class}: #{error.message}", defined?(sql) && sql&.join("\n"))
        warn "#{relative(path)}:#{location}: #{error.message}" if ENV['JED_NATIVE_VERBOSE'] == '1'
      end
    end
    raise 'blocked writer was never released' unless waiting.empty?
  ensure
    handles&.each_value(&:close)
    db&.close
    FileUtils.remove_entry(directory) if directory
  end
end
