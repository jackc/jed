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
    puts "Prepared #{files.length} complete pinned encoding fixture files"
    destination
  end
end
