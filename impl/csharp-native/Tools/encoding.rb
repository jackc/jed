# frozen_string_literal: true

require "bundler/setup"
require "toml-rb"
require_relative "tasks"

module CSharpNativeEncoding
  module_function

  # TOML is decoded only by the existing repository tooling. The C# checker
  # consumes JSON with exact integer tokens; no new package or fixture copy.
  def generate
    names = %w[integers text bytea decimal float interval range composite]
    spec = CSharpNativeTasks.baseline_spec
    files = names.map do |name|
      relative = "encoding/#{name}.toml"
      path = File.join(spec, relative)
      data = TomlRB.load_file(path)
      abort "Unexpected encoding schema in #{relative}" unless data.fetch("schema_version") == 1
      abort "Unexpected encoding sections in #{relative}" unless (data.keys - %w[schema_version bare nullable descending]).empty?
      { "source" => relative, "sha256" => Digest::SHA256.file(path).hexdigest, "data" => data }
    end
    destination = File.join(CSharpNativeTasks::BUILD, "encoding-vectors.json")
    FileUtils.mkdir_p(File.dirname(destination))
    File.write(destination, JSON.generate({ "semantic_revision" => CSharpNativeTasks::BASELINE.fetch("semantic_revision"), "files" => files }))
    temporal = %w[dates timestamps intervals].map do |name|
      relative = "encoding/#{name}.toml"
      path = File.join(spec, relative)
      data = TomlRB.load_file(path)
      abort "Unexpected temporal sections in #{relative}" unless (data.keys - %w[schema_version parse parse_error render]).empty?
      { "source" => relative, "sha256" => Digest::SHA256.file(path).hexdigest, "data" => data }
    end
    File.write(File.join(CSharpNativeTasks::BUILD, "temporal-vectors.json"), JSON.generate({ "semantic_revision" => CSharpNativeTasks::BASELINE.fetch("semantic_revision"), "files" => temporal }))
    timezone = %w[tzif bundle].map do |name|
      relative = "tz/vectors/#{name}.toml"
      path = File.join(spec, relative)
      { "source" => relative, "sha256" => Digest::SHA256.file(path).hexdigest, "data" => TomlRB.load_file(path) }
    end
    File.write(File.join(CSharpNativeTasks::BUILD, "timezone-vectors.json"), JSON.generate({ "semantic_revision" => CSharpNativeTasks::BASELINE.fetch("semantic_revision"), "files" => timezone }))
    collation = %w[compiler sortkey bundle].map do |name|
      relative = "collation/vectors/#{name}.toml"
      path = File.join(spec, relative)
      { "source" => relative, "sha256" => Digest::SHA256.file(path).hexdigest, "data" => TomlRB.load_file(path) }
    end
    File.write(File.join(CSharpNativeTasks::BUILD, "collation-vectors.json"), JSON.generate({ "semantic_revision" => CSharpNativeTasks::BASELINE.fetch("semantic_revision"), "files" => collation }))
    regex = %w[program_vectors match_vectors similar_vectors].map do |name|
      relative = "regex/#{name}.toml"
      path = File.join(spec, relative)
      { "source" => relative, "sha256" => Digest::SHA256.file(path).hexdigest, "data" => TomlRB.load_file(path) }
    end
    File.write(File.join(CSharpNativeTasks::BUILD, "regex-vectors.json"), JSON.generate({ "semantic_revision" => CSharpNativeTasks::BASELINE.fetch("semantic_revision"), "files" => regex }))
    gin = %w[gin_jsonb_terms gin_posting_runs].map do |name|
      relative = "encoding/#{name}.toml"
      path = File.join(spec, relative)
      { "source" => relative, "sha256" => Digest::SHA256.file(path).hexdigest, "data" => TomlRB.load_file(path) }
    end
    File.write(File.join(CSharpNativeTasks::BUILD, "gin-vectors.json"), JSON.generate({ "semantic_revision" => CSharpNativeTasks::BASELINE.fetch("semantic_revision"), "files" => gin }))
    memory = "cost/memory_sizes.toml"
    memory_path = File.join(spec, memory)
    File.write(File.join(CSharpNativeTasks::BUILD, "memory-vectors.json"), JSON.generate({ "semantic_revision" => CSharpNativeTasks::BASELINE.fetch("semantic_revision"), "source" => memory, "sha256" => Digest::SHA256.file(memory_path).hexdigest, "data" => TomlRB.load_file(memory_path) }))
    puts "Prepared #{files.length} complete pinned encoding fixture files"
    destination
  end
end
