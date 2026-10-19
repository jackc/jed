# frozen_string_literal: true

# Explicit, lossless post-processing for this experiment's JSON evidence.
# Timed workers keep their original writers; no serialization enters a timer.
require 'json'
require 'digest'
require 'optparse'

module LikeEvidenceJSON
  module_function

  def compact(path)
    original = File.binread(path)
    parsed = JSON.parse(original)
    canonical = JSON.generate(parsed)
    encoded = canonical + "\n"
    raise "JSON values changed: #{path}" unless JSON.parse(encoded) == parsed
    File.binwrite(path, encoded)
    { 'original_bytes' => original.bytesize, 'compact_bytes' => encoded.bytesize,
      'original_file_sha256' => Digest::SHA256.hexdigest(original),
      'compact_file_sha256' => Digest::SHA256.hexdigest(encoded),
      'parsed_json_sha256' => Digest::SHA256.hexdigest(canonical) }
  end

  def run(directory)
    ledger_path = File.join(directory, 'serialization.json')
    ledger = File.file?(ledger_path) ? JSON.parse(File.read(ledger_path)) : {
      'format' => 'UTF-8 JSON.generate(JSON.parse(original_bytes)) followed by one newline',
      'preservation' => 'Every parsed JSON value, including all semantic cases, metadata and cost-prefix outcomes, is preserved. No fields are removed or rounded. Log files remain byte-for-byte unchanged.',
      'files' => {}
    }
    Dir[File.join(directory, '*.json')].sort.each do |path|
      name = File.basename(path)
      next if %w[serialization.json manifest.json].include?(name)
      current_hash = Digest::SHA256.file(path).hexdigest
      recorded = ledger.fetch('files')[name]
      next if recorded && recorded.fetch('compact_file_sha256') == current_hash
      ledger.fetch('files')[name] = compact(path)
    end
    File.write(ledger_path, JSON.pretty_generate(ledger) + "\n")
    ledger
  end
end

if $PROGRAM_NAME == __FILE__
  options = { directory: __dir__ }
  OptionParser.new do |parser|
    parser.banner = 'Explicit evidence formatting: ruby compact.rb [--directory PATH]'
    parser.on('--directory PATH') { |value| options[:directory] = File.expand_path(value) }
  end.parse!
  abort 'Unexpected arguments' unless ARGV.empty?
  ledger = LikeEvidenceJSON.run(options[:directory])
  puts "Preserved and compacted #{ledger.fetch('files').length} JSON reports; logs unchanged."
end
