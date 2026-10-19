# frozen_string_literal: true

require_relative '../lib/jed_native'

module NativeRegexMemoryTests
  module_function

  def run
    ['aBc', 'Aé文z', 'Aé😀文z'].each do |text|
      view = JedNative::RegexText.new(text)
      characters = text.chars
      raise 'regex character offsets differ' unless view.length == characters.length && view.to_a == characters
      (0..characters.length).each do |first|
        (first..characters.length).each do |last|
          raise 'regex UTF-8 slice differs' unless view.slice(first, last) == characters[first...last].join
        end
      end
      folded = view.with_case(true)
      raise 'regex ASCII case fold differs' unless folded.to_a == text.tr('A-Z', 'a-z').chars
      raise 'regex case view copied UTF-8 offsets' unless folded.instance_variable_get(:@offsets).equal?(view.instance_variable_get(:@offsets))
    end
    ascii = JedNative::RegexText.new('a' * (8 * 1024 * 1024))
    raise 'ASCII input allocated a character index' if ascii.instance_variable_get(:@offsets)
    unicode = JedNative::RegexText.new('é😀' * 131_072)
    raise 'UTF-8 input index is not compact' unless unicode.instance_variable_get(:@offsets).bytesize == 4 * (unicode.length + 1)
    db = JedNative.memory
    source = 'aé😀' * 2_000
    result = db.execute("SELECT regexp_replace($1, '(a)(é)(😀)', '\\3\\2\\1', 'g')", source)
    raise 'global capture replacement differs' unless result.rows == [['😀éa' * 2_000]]
    empty = db.execute("SELECT regexp_replace($1, '', 'x', 'g')", 'é😀')
    raise 'empty Unicode replacement differs' unless empty.rows == [['xéx😀x']]
    puts 'ruby-native regex: compact ASCII/UTF-8 inputs and bounded replacement replay passed'
  ensure
    db&.close
  end
end

NativeRegexMemoryTests.run
