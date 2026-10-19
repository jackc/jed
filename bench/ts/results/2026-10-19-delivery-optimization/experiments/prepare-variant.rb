# frozen_string_literal: true

# Freeze the archived baseline, or copy it and replace only the named files from this checkout.
# Reconstruct retained experiments from patches instead; see reproduce.md.
require "digest"
require "fileutils"
require "json"
require "open3"
require "pathname"

BASELINE_COMMIT = "74ed390506e5443fa6793328194bd85ae441f204"

root = File.expand_path(ENV.fetch("JED_EXPERIMENT_REPO", "../../../../.."), __dir__)
results = File.expand_path(ENV.fetch("JED_EXPERIMENT_RESULTS", File.join(root, "bench/results/delivery-optimization-20261019")))
base = File.join(results, "sources/baseline")
label = ARGV.shift
abort "Usage: ruby prepare-variant.rb VARIANT [REPO_RELATIVE_FILE ...]" unless label
unless /\A[a-z][a-z0-9_]*\z/.match?(label)
  abort "Variant must be a lowercase label without path separators"
end
files = ARGV
abort "Duplicate source file" unless files.uniq == files
files.each do |file|
  path = Pathname.new(file)
  if path.absolute? || path.cleanpath.to_s != file || file.split("/").include?("..")
    abort "Expected a clean repository-relative file: #{file}"
  end
  abort "Missing baseline file: #{file}" unless File.file?(File.join(base, file))
  abort "Missing checkout file: #{file}" unless File.file?(File.join(root, file))
end
unless File.file?(File.join(base, "impl/ts/src/async/packed.ts"))
  abort "Extract the baseline archive first: #{base}"
end
abort "The baseline takes no replacement files" if label == "baseline" && !files.empty?

out = File.join(File.dirname(base), label)
if label != "baseline"
  abort "Snapshot exists: #{out}" if File.exist?(out)
  FileUtils.cp_r(base, out)
  files.each { |file| FileUtils.cp(File.join(root, file), File.join(out, file)) }
end

FileUtils.mkdir_p(File.join(out, "impl/ts"))
%w[node_modules jed_lock.node].each do |name|
  source = File.join(root, "impl/ts", name)
  target = File.join(out, "impl/ts", name)
  File.symlink(source, target) if File.exist?(source) && !File.exist?(target)
end
unless File.directory?(File.join(out, "impl/ts/node_modules"))
  abort "Install the existing impl/ts dependencies first; no downloads performed"
end
FileUtils.cp(
  File.join(root, "mise-tasks/bench/ts_async_browser"),
  File.join(out, "mise-tasks/bench/ts_async_browser")
)

paths = Dir[File.join(out, "impl/ts/src/**/*.ts")]
paths.concat(Dir[File.join(out, "bench/ts/**/*")].select { |path| File.file?(path) })
hashes = paths.sort.to_h do |path|
  [path.delete_prefix(out + "/"), Digest::SHA256.file(path).hexdigest]
end
patch = files.map do |file|
  text, error, status = Open3.capture3(
    "diff", "-U3", "--label", "a/#{file}", "--label", "b/#{file}",
    File.join(base, file), File.join(out, file)
  )
  abort error unless [0, 1].include?(status.exitstatus)
  text
end.join
manifest = {
  label: label, baselineCommit: BASELINE_COMMIT, files: files,
  sourceSha256: hashes, patchSha256: Digest::SHA256.hexdigest(patch)
}
File.write(File.join(out, "variant.patch"), patch)
File.write(File.join(out, "variant.json"), JSON.pretty_generate(manifest) + "\n")
puts out
