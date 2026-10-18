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
    puts "ruby-native regex: #{programs.length} program vectors and #{matches.length} match/cost vectors passed"
  end
end

NativeRegexTests.run
