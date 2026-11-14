# frozen_string_literal: true
require_relative "peer_actors"

# Encryption changes physical page capacity, so cost and EXPLAIN have an
# independent encrypted peer oracle (encryption.md §7), not plaintext answers.
module JavaEncryptedOracle
  module_function
  def prepare(files)
    peer = JavaPeerActors.go(:conformance)
    spec = JavaNativeTasks.baseline_spec
    suites = File.join(spec, "conformance/suites")
    sources = files.to_h do |path|
      relative = path.start_with?(suites + "/") ? path.delete_prefix(suites + "/") : "additional/#{Digest::SHA256.hexdigest(path)[0, 16]}-#{File.basename(path)}"
      [relative, [path, Digest::SHA256.file(path).hexdigest]]
    end
    identity = { format: 2, revision: JavaNativeTasks::BASELINE.fetch("semantic_revision"), peer_sha256: Digest::SHA256.file(peer).hexdigest, sources: sources.sort.to_h }
    key = Digest::SHA256.hexdigest(JSON.generate(identity))
    directory = File.join(JavaNativeTasks::DEV, "encrypted-oracles", key)
    FileUtils.mkdir_p(directory)
    File.open(File.join(directory, ".lock"), File::RDWR | File::CREAT, 0o600) do |lock|
      lock.flock(File::LOCK_EX)
      marker = File.join(directory, "complete.json")
      if File.file?(marker)
        JSON.parse(File.read(marker)).fetch("artifacts").each do |name, hash|
          abort "Encrypted peer evidence changed: #{name}" unless Digest::SHA256.file(File.join(directory, name)).hexdigest == hash
        end
      else
        root = File.join(directory, "corpus")
        FileUtils.mkdir_p(File.join(root, "spec/conformance/suites"))
        Dir.children(spec).each do |name|
          next if name == "conformance"
          FileUtils.ln_sf(File.join(spec, name), File.join(root, "spec", name))
        end
        Dir.children(File.join(spec, "conformance")).each do |name|
          next if name == "suites"
          FileUtils.ln_sf(File.join(spec, "conformance", name), File.join(root, "spec/conformance", name))
        end
        sources.each do |relative, (path, hash)|
          destination = File.join(root, "spec/conformance/suites", relative)
          FileUtils.mkdir_p(File.dirname(destination)); FileUtils.cp(path, destination)
          abort "Corpus changed during peer capture: #{path}" unless Digest::SHA256.file(destination).hexdigest == hash && Digest::SHA256.file(path).hexdigest == hash
        end
        puts "Checking encrypted cost/EXPLAIN oracle with pinned Go #{identity[:revision]} (#{sources.length} files)"
        $stdout.flush
        env = { "JED_CONFORMANCE_ENCRYPTED" => "1", "JED_CONFORMANCE_TRANSCRIPT" => File.join(directory, "events.tsv"), "JED_CONFORMANCE_FILTER" => nil,
                "JED_CONFORMANCE_TEMP_BUFFERS" => nil, "JED_CONFORMANCE_WORK_MEM" => nil, "JED_CONFORMANCE_QUERY_MEMORY" => nil }
        File.open(File.join(directory, "peer.log"), "w") do |output|
          abort "Pinned encrypted Go corpus failed; see #{directory}/peer.log" unless system(env, peer, "disk", chdir: root, out: output, err: output)
        end
        passed = File.readlines(File.join(directory, "peer.log")).filter_map { /\APASS (.+)\s*\z/.match(_1)&.[](1)&.strip }
        skipped = File.readlines(File.join(directory, "peer.log")).filter_map { /\ASKIP (.+?)  \((disk-mode|encrypted-mode)\)\s*\z/.match(_1)&.[](1) }
        abort "Pinned peer did not account for every selected file" unless (passed + skipped).sort == sources.keys.sort
        File.write(File.join(directory, "files.tsv"), passed.map { |relative| path, hash = sources.fetch(relative); "#{path}\t#{relative}\t#{hash}\n" }.join)
        hashes = %w[events.tsv files.tsv peer.log].to_h { [_1, Digest::SHA256.file(File.join(directory, _1)).hexdigest] }
        File.write(marker, JSON.pretty_generate(identity.merge(artifacts: hashes)) + "\n")
      end
    end
    puts "Encrypted peer evidence: #{directory}"
    ENV["JED_JAVA_ENCRYPTED_ORACLE"] = directory
  end
end
