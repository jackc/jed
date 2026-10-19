#!/usr/bin/env ruby
# frozen_string_literal: true

# Build a disposable profiling snapshot. No working-tree source is patched and no dependency
# installation occurs. Coarse instrumentation has no per-row or per-continuation-segment clock.
require "digest"
require "fileutils"
require "json"
require "open3"
require "time"

abort "Usage: ruby bench/ts/profile/prepare.rb SOURCE_ROOT OUTPUT_ROOT plain|coarse|detailed" unless ARGV.length == 3
source_root = File.realpath(ARGV[0])
output_root = File.expand_path(ARGV[1])
mode = ARGV[2]
abort "Mode must be plain, coarse or detailed" unless %w[plain coarse detailed].include?(mode)
abort "Output must be a new or empty directory" if File.exist?(output_root) && (!File.directory?(output_root) || !Dir.empty?(output_root))
abort "Output must not be inside the copied source tree" if output_root.start_with?(File.join(source_root, "impl/ts/src") + "/")

source_directory = File.join(source_root, "impl/ts/src")
package = File.join(source_root, "impl/ts/package.json")
workload = File.join(source_root, "bench/ts/src/async-heavy.ts")
dependencies = File.join(source_root, "impl/ts/node_modules")
abort "Source root must contain the TS core, package.json and shared heavy workload" unless File.directory?(source_directory) && File.file?(package) && File.file?(workload)
abort "Existing impl/ts/node_modules is required; no dependencies are downloaded" unless File.directory?(dependencies)
abort "Source root is already instrumented" if File.exist?(File.join(source_directory, "__profile.mjs"))
dependencies = File.realpath(dependencies)

def replace_exact(source, needle, replacement, label, occurrences: 1)
  count = source.scan(Regexp.new(Regexp.escape(needle))).length
  abort "#{label}: expected #{occurrences} patch points, found #{count}" unless count == occurrences
  source.gsub(needle, replacement)
end

def wrap_body(source, expression, label, stage)
  matches = source.scan(expression)
  abort "#{label}: expected one method body, found #{matches.length}" unless matches.length == 1
  source.sub(expression) do
    match = Regexp.last_match
    opening, body, closing = match[1], match[2], match[3]
    "#{opening}    const __profileStarted = begin(\"#{stage}\");\n    try {\n" +
      body.lines.map { |line| line.strip.empty? ? line : "  #{line}" }.join +
      "    } finally {\n      end(\"#{stage}\", __profileStarted);\n    }\n#{closing}"
  end
end

originals = {}
prepared = {}
%w[impl/ts/src/execution.ts impl/ts/src/async/packed.ts].each do |relative|
  originals[relative] = File.read(File.join(source_root, relative))
  prepared[relative] = originals[relative]
end

unless mode == "plain"
  execution = prepared.fetch("impl/ts/src/execution.ts")
  execution = replace_exact(execution, 'import { currentCancellationCheck, withCancellationScope } from "./cancel.ts";',
    "import { begin, end } from \"./__profile.mjs\";\nimport { currentCancellationCheck, withCancellationScope } from \"./cancel.ts\";", "execution import")
  schedule = 'await new Promise<void>((resolve) => schedule(resolve));'
  count = execution.scan(Regexp.new(Regexp.escape(schedule))).length
  abort "schedule waits: expected three patch points, found #{count}" unless count == 3
  execution = execution.gsub(/^([ \t]*)await new Promise<void>\(\(resolve\) => schedule\(resolve\)\);$/) do
    indent = Regexp.last_match[1]
    [
      'let __profileWait = -1;',
      'try {',
      '  await new Promise<void>((resolve) => {',
      '    schedule(resolve);',
      '    __profileWait = begin("schedule_wait");',
      '  });',
      '} finally {',
      '  end("schedule_wait", __profileWait);',
      '}',
    ].map { |line| indent + line }.join("\n")
  end
  abort "A scheduling patch point was not on its expected line" if execution.include?(schedule)
  execution = replace_exact(execution, "          input = await effect.async();", <<~TS.chomp.gsub(/^/, "          "), "async I/O wait")
              const __profileIo = begin("io_wait");
              try {
                input = await effect.async();
              } finally {
                end("io_wait", __profileIo);
              }
  TS
  prepared["impl/ts/src/execution.ts"] = execution

  packed = prepared.fetch("impl/ts/src/async/packed.ts")
  packed = "import { begin, end } from \"../__profile.mjs\";\n" + packed
  decode_open = "export function decodePackedRows(packet: PackedRows, budget = new WireBudget()): DecodedPackedRows {\n"
  abort "decode entry: expected one patch point" unless packed.scan(Regexp.new(Regexp.escape(decode_open))).length == 1
  decode_at = packed.index(decode_open)
  abort "decode body must be the final function" unless decode_at && packed.end_with?("\n}\n")
  decode_body = packed[(decode_at + decode_open.length)...-2]
  decode_body = replace_exact(decode_body, "  const reader = new PackedReader(data);", <<~TS.chomp.gsub(/^/, "  "), "decode pass boundary")
      end(__profileStage, __profileStarted);
      __profileStage = "decode_construct";
      __profileStarted = begin(__profileStage);
      const reader = new PackedReader(data);
  TS
  decode = decode_open + <<~TS.lines.map { |line| "  #{line}" }.join
    let __profileStage = "decode_validate";
    let __profileStarted = begin(__profileStage);
    try {
  TS
  decode += decode_body.lines.map { |line| line.strip.empty? ? line : "  #{line}" }.join
  decode += "  } finally {\n    end(__profileStage, __profileStarted);\n  }\n}\n"
  packed = replace_exact(packed, packed[decode_at..], decode, "decode body")

  if mode == "detailed"
    packed = replace_exact(packed, "      size = measureValues(row, new WireBudget(this.maxBytes - ENVELOPE_BYTES));", <<~TS.chomp.gsub(/^/, "      "), "row preflight")
          const __profileMeasure = begin("encode_measure");
          try {
            size = measureValues(row, new WireBudget(this.maxBytes - ENVELOPE_BYTES));
          } finally {
            end("encode_measure", __profileMeasure);
          }
    TS
    packed = wrap_body(packed, /(  append\(row: Value\[\]\): boolean \{\n)(.*?)(  \}\n)(?=\n  finish\()/m, "append body", "encode_append")
    packed = wrap_body(packed, /(  finish\(\): PackedRows \{\n)(.*?)(  \}\n)(?=\n  private grow\()/m, "finish body", "encode_finish")
  end
  prepared["impl/ts/src/async/packed.ts"] = packed
end

paths = Dir.glob(File.join(source_directory, "**/*")).select { |path| File.file?(path) }
paths = paths.map { |path| path.delete_prefix(source_root + "/") }
paths += %w[impl/ts/package.json bench/ts/src/async-heavy.ts]
paths.sort!
source_hashes = paths.to_h { |path| [path, Digest::SHA256.file(File.join(source_root, path)).hexdigest] }
originals.each do |path, text|
  abort "Source changed while preparing #{path}" unless Digest::SHA256.hexdigest(text) == source_hashes.fetch(path)
end

FileUtils.mkdir_p(File.join(output_root, "impl/ts"))
FileUtils.mkdir_p(File.join(output_root, "bench/ts/src"))
FileUtils.cp_r(source_directory, File.join(output_root, "impl/ts/src"))
FileUtils.cp(package, File.join(output_root, "impl/ts/package.json"))
FileUtils.cp(workload, File.join(output_root, "bench/ts/src/async-heavy.ts"))
File.symlink(dependencies, File.join(output_root, "impl/ts/node_modules"))
source_hashes.each do |path, hash|
  abort "Source changed while copying #{path}" unless Digest::SHA256.file(File.join(output_root, path)).hexdigest == hash
end
prepared.each { |relative, text| File.write(File.join(output_root, relative), text) }
FileUtils.cp(File.join(__dir__, "stages.mjs"), File.join(output_root, "impl/ts/src/__profile.mjs"))

source_hashes.each do |path, hash|
  abort "Source changed while preparing snapshot: #{path}" unless Digest::SHA256.file(File.join(source_root, path)).hexdigest == hash
end
snapshot_hashes = (paths + ["impl/ts/src/__profile.mjs"]).sort.to_h do |path|
  [path, Digest::SHA256.file(File.join(output_root, path)).hexdigest]
end
changed = snapshot_hashes.keys.select { |path| snapshot_hashes[path] != source_hashes[path] }
patch = changed.map do |path|
  before = source_hashes.key?(path) ? File.join(source_root, path) : "/dev/null"
  label = source_hashes.key?(path) ? "a/#{path}" : "/dev/null"
  text, error, status = Open3.capture3("diff", "-U3", "--label", label, "--label", "b/#{path}", before, File.join(output_root, path))
  abort "Cannot create snapshot patch: #{error}" unless [0, 1].include?(status.exitstatus)
  text
end.join
File.write(File.join(output_root, "profile.patch"), patch)
manifest = {
  schemaVersion: 1, mode: mode, preparedAt: Time.now.utc.iso8601,
  sourceRoot: source_root, outputRoot: output_root, dependencies: dependencies,
  sourceSha256: source_hashes, snapshotSha256: snapshot_hashes,
  toolSha256: Dir.glob(File.join(__dir__, "*")).select { |path| File.file?(path) }.sort.to_h { |path| [File.basename(path), Digest::SHA256.file(path).hexdigest] },
  changedFiles: changed, patchSha256: Digest::SHA256.hexdigest(patch),
  stages: {
    schedule_wait: "After schedule(resolve) returns through resumption after await; excludes synchronous enqueue work.",
    io_wait: "Async provider invocation through resumption after await; includes its synchronous invocation prefix.",
    decode_validate: "Packed packet envelope checks and complete first validation pass; excludes AsyncRows reply checks.",
    decode_construct: "Complete second pass, owned value construction and result arrays.",
    encode_append: "Detailed only: inclusive row admission, preflight, growth and binary writing; rejected attempts count.",
    encode_measure: "Detailed only: measureValues plus its WireBudget allocation; a subset of encode_append.",
    encode_finish: "Detailed only: header finalization and exact-length buffer trimming.",
  },
  notes: [
    "Stage sums use milliseconds; counters are local to each JS realm and reset explicitly.",
    "Plain installs the counter module but leaves every original engine source byte unchanged.",
    "Coarse has no per-row timers. Detailed intentionally adds row timing and must be calibrated for observer overhead.",
    "GC inside a measured interval is included, not an additional additive stage.",
    "No parse, planning, consumer or per-continuation active timers are installed.",
  ],
}
File.write(File.join(output_root, "profile-manifest.json"), JSON.pretty_generate(manifest) + "\n")
puts JSON.generate({ mode: mode, outputRoot: output_root, changedFiles: changed })
