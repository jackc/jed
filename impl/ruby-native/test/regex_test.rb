# frozen_string_literal: true

require_relative '../lib/jed_native'
require_relative '../../../scripts/lib/bundle_setup'
require 'toml-rb'

# Instruction order and internal match costs are canonical bytecode invariants
# that SQL transcripts cannot expose. Read the shared vectors directly.
module NativeRegexTests
  ROOT = File.expand_path('../../..', __dir__)
  Meter = Struct.new(:cost) do
    def charge(amount = 1) = self.cost += amount
  end
  module_function

  def program(example)
    flags = example.fetch('flags', '')
    JedNative::RegexProgram.new(example.fetch('pattern'), insensitive: flags.include?('i'), multiline: flags.include?('m'))
  end

  def run
    programs = TomlRB.load_file(File.join(ROOT, 'spec/regex/program_vectors.toml')).fetch('case')
    programs.each do |example|
      compiled = program(example)
      classes = []
      names = { any_char: 'anychar', jump: 'jmp', start: 'assertstart', end: 'assertend', line_start: 'assertlinestart', line_end: 'assertlineend' }
      listing = compiled.code.map do |instruction|
        kind, *args = instruction
        if kind == :class
          number = classes.length
          classes << (args[0] ? '^' : '') + args[1].map { |low, high| "#{low}-#{high}" }.join(',')
          "class #{number}"
        else
          args[0] = args[0].ord if kind == :char
          ([names.fetch(kind, kind.to_s)] + args).join(' ')
        end
      end
      raise "regex program vector #{example['pattern'].inspect}" unless listing == example['prog'] && listing.length == example['count'] && classes == example.fetch('classes', [])
    end
    matches = TomlRB.load_file(File.join(ROOT, 'spec/regex/match_vectors.toml')).fetch('case')
    matches.each do |example|
      compiled, meter = program(example), Meter.new(0)
      result = compiled.search(compiled.input(example.fetch('input')), 0, context: meter)
      captures = result ? (0..compiled.groups).map { |group| compiled.span(result, group) || [-1, -1] } : []
      unless !result.nil? == example['matched'] && captures == example['caps'] && meter.cost == example['steps']
        raise "regex match vector #{example['pattern'].inspect}: #{[captures, meter.cost].inspect} expected #{[example['caps'], example['steps']].inspect}"
      end
    end
    similar = TomlRB.load_file(File.join(ROOT, 'spec/regex/similar_vectors.toml')).fetch('case')
    similar.each { |example| similar_vector(example) }
    puts "ruby-native regex: #{programs.length} program vectors, #{matches.length} match/cost vectors and #{similar.length} SIMILAR vectors passed"
  end

  def sqlstate
    yield
    nil
  rescue JedNative::Error => error
    error.sqlstate
  end

  def similar_vector(example)
    pattern, escape, input = example.fetch('pattern'), example['escape'], example.fetch('input')
    label = "SIMILAR vector #{pattern.inspect}"
    if example['error']
      raise "#{label}: expected #{example['error']}" unless sqlstate { JedNative::Similar.translate(pattern, escape) } == example['error']
      return
    end
    translated = JedNative::Similar.translate(pattern, escape)
    program, meter = JedNative::Similar.compile(translated), Meter.new(0)
    matched = program.match?(input, context: meter)
    actual = [translated, matched, program.code.length, meter.cost]
    expected = example.values_at('translation', 'similar', 'similar_compile', 'similar_steps')
    raise "#{label}: #{actual.inspect} expected #{expected.inspect}" unless actual == expected
    if example['substring_error']
      raise "#{label}: expected substring #{example['substring_error']}" unless sqlstate { JedNative::Similar::Matcher.new(pattern, escape || '\\') } == example['substring_error']
      return
    end
    matcher, meter = JedNative::Similar::Matcher.new(pattern, escape || '\\'), Meter.new(0)
    original = JedNative::RegexText.new(input)
    span = matcher.span(original, meter)
    actual = [span && original.slice(*span), matcher.instructions, meter.cost]
    expected = [example['substring_null'] ? nil : example['substring'], example['substring_compile'], example['substring_steps']]
    raise "#{label}: substring #{actual.inspect} expected #{expected.inspect}" unless actual == expected
  end
end

NativeRegexTests.run
