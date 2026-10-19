# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

# The shared window corpus pins SQL behavior. This host invariant checks that a
# small window prefix does not instantiate a row view for the whole paged table.
module NativeWindowPrefixTests
  module_function

  def assert(value, message)
    raise message unless value
  end

  def query(session, sql, expected, scanned)
    views = 0
    trace = TracePoint.new(:call) do |event|
      views += 1 if event.defined_class == JedNative::PagedRows && event.method_id == :row_at
    end
    result = nil
    trace.enable { result = session.query(sql); assert(result.rows == expected, "wrong prefix: #{sql}") }
    assert(views == scanned, "prefix read #{views} row views; expected #{scanned}")
    result.cost
  end

  def run
    Dir.mktmpdir('jed-ruby-window-prefix') do |directory|
      JedNative.create(File.join(directory, 'db'), page_size: 512, locking: :exclusive) do |database|
        session = database.session
        session.execute('CREATE TABLE t(a i32, b i32, v i32, PRIMARY KEY(a,b))')
        # Insert out of key order, and span several physical leaves.
        values = 100.times.to_a.reverse.map { |index| "(#{index / 10},#{index % 10},#{index + 1})" }.join(',')
        session.execute("INSERT INTO t VALUES #{values}")
        pages = session.engine.send(:full_scan_pages, session.engine.tables.fetch('t'))
        assert(pages > 1, 'prefix fixture needs multiple pages')
        cost = query(session,
          'SELECT a,b,sum(v) OVER (ORDER BY a,b ROWS UNBOUNDED PRECEDING) FROM t ORDER BY a,b LIMIT 2 OFFSET 1',
          [[0, 1, 3], [0, 2, 6]], 3)
        assert(cost == pages + 11, 'ascending prefix page/row/window cost changed')
        cost = query(session,
          'SELECT a,b,sum(v) OVER (ORDER BY a DESC,b DESC ROWS UNBOUNDED PRECEDING) FROM t ORDER BY a DESC,b DESC LIMIT 2 OFFSET 1',
          [[9, 8, 199], [9, 7, 297]], 3)
        assert(cost == pages + 11, 'descending prefix page/row/window cost changed')
        cost = query(session,
          'SELECT a,b,count(*) OVER (ORDER BY a,b ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY a,b LIMIT 3',
          [[0, 0, 1], [0, 1, 2], [0, 2, 2]], 3)
        assert(cost == pages + 13, 'moving prefix cost changed')
        cost = query(session,
          'SELECT row_number() OVER (ORDER BY a,b) FROM t ORDER BY a,b LIMIT 0 OFFSET 50', [], 0)
        assert(cost == pages, 'LIMIT 0 did row work or lost the full page block')
        session.close
      end
    end
    puts 'ruby-native window prefix: bounded paged reads, composite ASC/DESC keys, OFFSET, LIMIT 0 and canonical costs passed'
  end
end

NativeWindowPrefixTests.run
