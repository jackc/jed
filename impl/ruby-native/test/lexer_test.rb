# frozen_string_literal: true

require_relative '../lib/jed_native/error'
require_relative '../lib/jed_native/lexer'

# The shared SQL corpus covers the grammar. These probes pin the scanner's
# byte offsets and token boundaries, including ambiguous operator prefixes.
module NativeLexerTests
  module_function

  def assert(value, message) = (raise(message) unless value)

  def scan(sql) = JedNative::Lexer.new(sql).tokens

  def values(sql)
    scan(sql).reject { |token| token.kind == :eof }.map(&:value)
  end

  def error(code, sql)
    scan(sql)
    raise "expected #{code} for #{sql.inspect}"
  rescue JedNative::Error => caught
    assert(caught.sqlstate == code, "expected #{code}, got #{caught.sqlstate} for #{sql.inspect}")
  end

  def operators
    {
      'a->>b->c #>> #> #-' => %w[a ->> b -> c #>> #> #-],
      'a!~*b!~c!=d <> e' => %w[a !~* b !~ c <> d <> e],
      'a-|-b || c && d' => %w[a -|- b || c && d],
      '< <= << <@ > >= >> @> @? @@' => %w[< <= << <@ > >= >> @> @? @@],
      '? ?| ?& &< &> ~ ~* :: => :' => %w[? ?| ?& &< &> ~ ~* :: => :],
      '(),.[]+-*/%=;' => %w[( ) , . [ ] + - * / % = ;],
      '->>> -|-' => %w[->> > -|-],
      'SELECT f($1::i64), 0x_FF, 0o17, 0b10, .5, 1e+2' =>
        ['select', 'f', '(', 1, '::', 'i64', ')', ',', 255, ',', 15, ',', 2, ',', '.5', ',', '1e+2']
    }.each do |sql, expected|
      assert(values(sql) == expected, "wrong operator boundary: #{sql.inspect}")
    end
    ['!', '#', '&', '|', '@', '-|', 'a!'].each { |sql| error('42601', sql) }
    %w[$0 $01 $65536 0x 1__0].each { |sql| error('42601', sql) }
  end

  def comments
    sql = " \t\r\nSELECT/* outer / * -- /* inner */ tail */1-- !~*\r\n+2/*done*/"
    assert(values(sql) == ['select', 1, '+', 2], 'nested/line comment boundaries changed')
    assert(values('-- no terminating newline').empty?, 'line comment at EOF produced a token')
    assert(values('/**/ / * - - /* /**/ */') == %w[/ * - -], 'comment prefixes consumed isolated punctuation')
    assert(values("'/* quoted */' \"-- quoted\"") == ['/* quoted */', '-- quoted'], 'comment marker inside quotes was skipped')
    error('42601', '/*')
    error('42601', '/* outer /* inner */')
    error('42601', '/* tail *')
  end

  def offsets_and_encoding
    sql = "SELECT 'é雪', \"té\"/*雪*/->>'雪'"
    expected = [[:word, 'select', 0], [:string, 'é雪', 7], [:symbol, ',', 14],
                [:identifier, 'té', 16], [:symbol, '->>', 28], [:string, '雪', 31],
                [:eof, nil, 36]]
    assert(scan(sql).map(&:to_a) == expected, 'offsets must remain bytes, including after UTF-8 comments')
    assert(values("U&'d\\0061t'/* comment */UESCAPE '\\'") == ['dat'], 'Unicode escape lookahead changed')
    assert(values("'a''b' \"a\"\"b\"") == ["a'b", 'a"b'], 'escaped quote boundaries changed')
    error('22021', "SELECT '\xFF'".dup.force_encoding(Encoding::UTF_8))
    error('42601', "SELECT '\0'")
    error('42601', "'unfinished")
    error('42622', '"' + '雪' * 22 + '"')
  end

  def run
    operators
    comments
    offsets_and_encoding
    puts 'ruby-native lexer: maximal operators, punctuation, comments, UTF-8 byte offsets and invalid input passed'
  end
end

NativeLexerTests.run
