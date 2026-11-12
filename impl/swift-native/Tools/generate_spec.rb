#!/usr/bin/env ruby
# frozen_string_literal: true

# Mechanical metadata only: no parser/planner/executor/codec algorithm generation.
# Read the recorded baseline's Git blobs, so future supported-core changes do not
# silently update this frozen experiment. Deliberate rebasing requires a reviewed
# baseline change and re-verification of the experiment's complete scope.
require_relative "../../../scripts/lib/bundle_setup"
require "toml-rb"
require "digest"
require "json"
require "open3"
require "fileutils"

module SwiftSpecGenerator
  ROOT = File.expand_path("../../..", __dir__)
  BASELINE = "0b76dd6e9c2675b552eb8af9e820065351ffa4b3"
  INPUTS = %w[
    spec/integrity/findings.toml
    spec/types/scalars.toml
    spec/types/ranges.toml
    spec/types/compare.toml
    spec/types/casts.toml
    spec/functions/catalog.toml
    spec/functions/coercion_snapshot.toml
    spec/errors/registry.toml
    spec/cost/schedule.toml
    spec/cost/estimator.toml
    spec/cost/estimator_vectors.toml
    spec/types/datetime_units.toml
    spec/types/formatting.toml
    spec/grammar/pg_keywords.toml
    spec/fileformat/lz4_vectors.toml
    spec/encoding/prng.toml
    spec/encoding/decimal.toml
    spec/encoding/integers.toml
    spec/encoding/text.toml
    spec/encoding/bytea.toml
    spec/encoding/timestamps.toml
    spec/encoding/dates.toml
    spec/encoding/intervals.toml
    spec/encoding/interval.toml
    spec/encoding/float.toml
    spec/encoding/range.toml
    spec/encoding/composite.toml
    spec/encoding/ordered.toml
    spec/encoding/gin_posting_runs.toml
    spec/encoding/gin_jsonb_terms.toml
    spec/tz/vectors/bundle.toml
    spec/tz/vectors/tzif.toml
    spec/fileformat/encryption_vectors.toml
    spec/pageservice/messages.toml
    spec/pageservice/fixtures.toml
    spec/regex/program_vectors.toml
    spec/regex/match_vectors.toml
    spec/regex/similar_vectors.toml
    spec/collation/vectors/bundle.toml
    spec/collation/vectors/compiler.toml
    spec/collation/vectors/sortkey.toml
  ].freeze
  TEXT_INPUTS = %w[spec/errors/detail_cases.txt spec/errors/position_cases.txt].freeze
  fixture_listing, fixture_error, fixture_status = Open3.capture3(
    "git", "ls-tree", "-r", "--name-only", BASELINE, "spec/fileformat/fixtures", chdir: ROOT
  )
  abort "cannot list frozen byte fixtures: #{fixture_error}" unless fixture_status.success?
  BYTE_INPUTS = (fixture_listing.lines.map(&:strip).select { |path| path.end_with?(".jed") }.sort +
    %w[spec/tz/fixtures/tzdata.jtz spec/collation/fixtures/unicode.jucd
       spec/collation/fixtures/unicode.coll spec/collation/fixtures/es.coll
       spec/collation/fixtures/dev-root.allkeys spec/collation/fixtures/dev-nordic.ldml
       spec/collation/17.0.0/root.allkeys spec/collation/17.0.0/es.ldml
       spec/collation/17.0.0/casing.txt]).freeze
  corpus_listing, corpus_error, corpus_status = Open3.capture3(
    "git", "ls-tree", "-r", "--name-only", BASELINE,
    "spec/conformance/suites", chdir: ROOT
  )
  abort "cannot list frozen conformance corpus: #{corpus_error}" unless corpus_status.success?
  CORPUS_INPUTS = corpus_listing.lines.map(&:strip).select { |path| path.end_with?(".test") }.sort.freeze
  abort "frozen corpus inventory changed" unless CORPUS_INPUTS.length == 749 &&
    CORPUS_INPUTS.count { |path| path.include?("/concurrency/") } == 7

  process_listing, process_error, process_status = Open3.capture3(
    "git", "ls-tree", "-r", "--name-only", BASELINE,
    "spec/conformance/process", chdir: ROOT
  )
  abort "cannot list frozen process corpus: #{process_error}" unless process_status.success?
  PROCESS_INPUTS = process_listing.lines.map(&:strip).select { |path| path.end_with?(".process.toml") }.sort.freeze
  abort "frozen process inventory changed" unless PROCESS_INPUTS.length == 31

  simulation_listing, simulation_error, simulation_status = Open3.capture3(
    "git", "ls-tree", "-r", "--name-only", BASELINE,
    "spec/conformance/sim/regress", "spec/conformance/sim/profiles", chdir: ROOT
  )
  abort "cannot list frozen simulation inputs: #{simulation_error}" unless simulation_status.success?
  SIMULATION_INPUTS = simulation_listing.lines.map(&:strip).select { |path| path.end_with?(".sim", ".toml") }.sort.freeze
  abort "frozen simulation inventory changed" unless SIMULATION_INPUTS.count { |path| path.end_with?(".sim") } == 18 &&
    SIMULATION_INPUTS.count { |path| path.end_with?(".toml") } == 22

  integrity_listing, integrity_error, integrity_status = Open3.capture3(
    "git", "ls-tree", "-r", "--name-only", BASELINE, "spec/fileformat/integrity", chdir: ROOT
  )
  abort "cannot list frozen integrity fixtures: #{integrity_error}" unless integrity_status.success?
  INTEGRITY_INPUTS = integrity_listing.lines.map(&:strip).select { |path| path.end_with?(".jed", ".json") }.sort.freeze
  abort "frozen integrity inventory changed" unless INTEGRITY_INPUTS.count { |path| path.end_with?(".jed") } == 24 &&
    INTEGRITY_INPUTS.count { |path| path.end_with?(".json") } == 24

  module_function

  def swift_string(value)
    # Swift uses \u{...}, not JSON's \uNNNN. Escape backslashes first by emitting
    # each Unicode scalar, so metadata containing \( cannot become interpolation.
    body = value.each_codepoint.map do |code|
      case code
      when 34 then '\"'
      when 92 then "\\\\"
      when 10 then "\\n"
      when 13 then "\\r"
      when 9 then "\\t"
      when 0..31, 127 then "\\u{#{code.to_s(16)}}"
      else code.chr(Encoding::UTF_8)
      end
    end.join
    '"' + body + '"'
  end

  def strings(items) = "[#{items.map { |item| swift_string(item) }.join(', ')}]"

  def value(data)
    case data
    when String then ".text(#{swift_string(data)})"
    when Integer
      raise "integer outside Int64: #{data}" unless (-2**63...2**63).cover?(data)
      ".integer(#{data})"
    when true, false then ".boolean(#{data})"
    when Array then ".array([#{data.map { |item| value(item) }.join(', ')}])"
    when Hash then ".table(#{table(data)})"
    else raise "unsupported spec value #{data.inspect}"
    end
  end

  def table(data)
    return "[:]" if data.empty?
    "[#{data.keys.sort.map { |key| "#{swift_string(key)}: #{value(data.fetch(key))}" }.join(', ')}]"
  end

  def array(name, type, rows)
    body = rows.map { |row| "        #{yield(row)}," }.join("\n")
    "    static let #{name}: [#{type}] = [\n#{body}\n    ]\n"
  end

  def read_sources
    (INPUTS + TEXT_INPUTS + BYTE_INPUTS + CORPUS_INPUTS + PROCESS_INPUTS + SIMULATION_INPUTS + INTEGRITY_INPUTS).to_h do |path|
      bytes, error, status = Open3.capture3("git", "show", "#{BASELINE}:#{path}", chdir: ROOT)
      abort "cannot read baseline #{path}: #{error}" unless status.success?
      [path, bytes.b]
    end
  end

  def generated(sources, documents, fingerprint)
    scalar = documents.fetch("spec/types/scalars.toml").fetch("type")
    errors = documents.fetch("spec/errors/registry.toml").fetch("error")
    schedule = documents.fetch("spec/cost/schedule.toml")
    catalog = documents.fetch("spec/functions/catalog.toml")
    ranges = documents.fetch("spec/types/ranges.toml").fetch("range")
    text = <<~SWIFT
      // Generated by Tools/generate_spec.rb. Mechanical spec data only; do not edit.
      // Regenerate/check with `mise run swift-native:codegen [--check]`.
      // Inputs are pinned Git blobs, not the evolving supported-core working tree.

      indirect enum SpecDataValue: Equatable, Sendable {
          case text(String)
          case integer(Int64)
          case boolean(Bool)
          case array([SpecDataValue])
          case table([String: SpecDataValue])
      }

      struct ScalarSpecFact: Equatable, Sendable {
          let id: String
          let aliases: [String]
          let family: String
          let storable: Bool
          let metadata: [String: SpecDataValue]
      }

      struct ErrorSpecFact: Equatable, Sendable {
          let code: String
          let name: String
          let errorClass: String
          let template: String
      }

      struct CostSpecFact: Equatable, Sendable {
          let id: String
          let weight: Int64
          let event: String
      }

      struct FunctionSpecFact: Equatable, Sendable {
          let category: String
          let name: String
          let kind: String
          let metadata: [String: SpecDataValue]
      }

      struct RangeSpecFact: Equatable, Sendable {
          let id: String
          let element: String
          let aliases: [String]
          let discrete: Bool
      }

      enum GeneratedSpec {
          static let baseline = #{swift_string(BASELINE)}
          static let fingerprint = #{swift_string(fingerprint)}
          static let sourceHashes: [String: String] = [
      #{sources.map { |path, bytes| "        #{swift_string(path)}: #{swift_string(Digest::SHA256.hexdigest(bytes))}," }.join("\n")}
          ]
          static let defaultScalarBytes: Int64 = #{schedule.fetch('limits').fetch('default_scalar_bytes')}
          static let memoryRow: Int64 = #{schedule.fetch('memory').fetch('row')}
          static let memoryValue: Int64 = #{schedule.fetch('memory').fetch('value')}
          static let memoryArrayDimension: Int64 = #{schedule.fetch('memory').fetch('array_dim')}
          static let memoryDecimalGroup: Int64 = #{schedule.fetch('memory').fetch('decimal_group')}
          static let memoryEntry: Int64 = #{schedule.fetch('memory').fetch('entry')}
          static let compactMinimumPages: Int64 = #{schedule.fetch('reclamation').fetch('compact_min_pages')}
          static let compactGrowth: Int64 = #{schedule.fetch('reclamation').fetch('compact_growth')}

    SWIFT
    text << array("scalars", "ScalarSpecFact", scalar) do |row|
      ".init(id: #{swift_string(row.fetch('id'))}, aliases: #{strings(row.fetch('aliases'))}, family: #{swift_string(row.fetch('family'))}, storable: #{row.fetch('storable')}, metadata: #{table(row)})"
    end
    text << array("errors", "ErrorSpecFact", errors) do |row|
      ".init(code: #{swift_string(row.fetch('code'))}, name: #{swift_string(row.fetch('name'))}, errorClass: #{swift_string(row.fetch('class'))}, template: #{swift_string(row.fetch('template'))})"
    end
    text << array("costs", "CostSpecFact", schedule.fetch("unit")) do |row|
      ".init(id: #{swift_string(row.fetch('id'))}, weight: #{row.fetch('weight')}, event: #{swift_string(row.fetch('event'))})"
    end
    categories = %w[operator aggregate window set_returning]
    functions = categories.flat_map { |category| catalog.fetch(category).map { |row| [category, row] } }
    # Bound each compiler initializer. This changes only source grouping; authored
    # catalog order and complete row metadata are preserved in the resulting table.
    groups = functions.each_slice(48).to_a
    groups.each_with_index do |rows, index|
      text << array("functionGroup#{index}", "FunctionSpecFact", rows) do |category, row|
        ".init(category: #{swift_string(category)}, name: #{swift_string(row.fetch('name'))}, kind: #{swift_string(row.fetch('kind'))}, metadata: #{table(row)})"
      end
    end
    text << "    static let functions: [FunctionSpecFact] = [#{groups.each_index.map { |i| "functionGroup#{i}" }.join(', ')}].flatMap { $0 }\n"
    text << array("ranges", "RangeSpecFact", ranges) do |row|
      ".init(id: #{swift_string(row.fetch('id'))}, element: #{swift_string(row.fetch('element'))}, aliases: #{strings(row.fetch('aliases'))}, discrete: #{row.fetch('discrete')})"
    end
    supplementary = {
      "integrity" => "spec/integrity/findings.toml",
      "comparison" => "spec/types/compare.toml",
      "casts" => "spec/types/casts.toml",
      "estimator" => "spec/cost/estimator.toml",
      "datetimeUnits" => "spec/types/datetime_units.toml",
      "formatting" => "spec/types/formatting.toml",
      "keywords" => "spec/grammar/pg_keywords.toml"
    }
    supplementary.each do |name, path|
      text << "    static let #{name}: [String: SpecDataValue] = #{table(documents.fetch(path))}\n"
    end
    text << "}\n"
  end

  def run(arguments)
    abort "usage: generate_spec.rb [--check]" unless arguments.empty? || arguments == ["--check"]
    sources = read_sources
    fingerprint = Digest::SHA256.hexdigest(sources.map { |path, bytes| path.b + "\0" + bytes + "\0" }.join)
    documents = INPUTS.to_h { |path| [path, TomlRB.parse(sources.fetch(path).dup.force_encoding(Encoding::UTF_8))] }
    snapshot = {
      "baseline" => BASELINE, "fingerprint" => fingerprint,
      "source_hashes" => sources.transform_values { |bytes| Digest::SHA256.hexdigest(bytes) },
      "documents" => documents
    }
    outputs = {
      "impl/swift-native/Sources/Jed/GeneratedSpec.swift" => generated(sources, documents, fingerprint),
      "impl/swift-native/Tests/JedTests/Fixtures/SpecSnapshot.json" => JSON.pretty_generate(snapshot) + "\n",
      "impl/swift-native/Tests/JedCryptoTests/Fixtures/encryption_vectors.toml" => sources.fetch("spec/fileformat/encryption_vectors.toml")
    }
    INTEGRITY_INPUTS.each do |path|
      outputs["impl/swift-native/Tests/JedTests/Fixtures/Integrity/#{File.basename(path)}"] = sources.fetch(path)
    end
    integrity_extras = Dir.glob("impl/swift-native/Tests/JedTests/Fixtures/Integrity/*.{jed,json}", base: ROOT) - outputs.keys
    abort "unexpected Swift frozen integrity fixtures: #{integrity_extras.join(', ')}" unless integrity_extras.empty?
    (BYTE_INPUTS + TEXT_INPUTS).each do |path|
      outputs["impl/swift-native/Tests/JedTests/Fixtures/#{File.basename(path)}"] = sources.fetch(path)
    end
    CORPUS_INPUTS.each do |path|
      relative = path.delete_prefix("spec/conformance/suites/")
      outputs["impl/swift-native/Tests/JedTests/Fixtures/Corpus/#{relative}"] = sources.fetch(path)
    end
    PROCESS_INPUTS.each do |path|
      outputs["impl/swift-native/Tools/Fixtures/Process/#{File.basename(path)}"] = sources.fetch(path)
    end
    SIMULATION_INPUTS.each do |path|
      relative = path.delete_prefix("spec/conformance/sim/")
      outputs["impl/swift-native/Tests/JedTests/Fixtures/Simulation/#{relative}"] = sources.fetch(path)
    end
    simulation_root = "impl/swift-native/Tests/JedTests/Fixtures/Simulation"
    simulation_extras = Dir.glob("#{simulation_root}/**/*.{sim,toml}", base: ROOT) - outputs.keys
    abort "unexpected Swift frozen simulation files: #{simulation_extras.join(', ')}" unless simulation_extras.empty?
    process_root = "impl/swift-native/Tools/Fixtures/Process"
    process_extras = Dir.glob("#{process_root}/*.process.toml", base: ROOT) - outputs.keys
    abort "unexpected Swift frozen process files: #{process_extras.join(', ')}" unless process_extras.empty?
    # Keep future accidental working-tree corpus copies out of this immutable
    # snapshot. This check never removes files; it reports the unexpected paths.
    corpus_root = "impl/swift-native/Tests/JedTests/Fixtures/Corpus"
    extras = Dir.glob("#{corpus_root}/**/*.test", base: ROOT) - outputs.keys
    abort "unexpected Swift frozen corpus files: #{extras.join(', ')}" unless extras.empty?
    stale = outputs.keys.select do |path|
      target = File.join(ROOT, path)
      if arguments == ["--check"]
        !File.file?(target) || File.binread(target) != outputs.fetch(path).b
      else
        FileUtils.mkdir_p(File.dirname(target))
        File.binwrite(target, outputs.fetch(path))
        false
      end
    end
    abort "stale Swift spec data: #{stale.join(', ')}" unless stale.empty?
    puts "Swift spec data #{arguments.empty? ? 'generated' : 'verified'}: #{BASELINE}, #{fingerprint}"
  end
end

SwiftSpecGenerator.run(ARGV) if $PROGRAM_NAME == __FILE__
