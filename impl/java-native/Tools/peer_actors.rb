# frozen_string_literal: true
require_relative "tasks"

# Independent test peer, exported from the experiment's immutable semantic revision.
# The approved existing Go module cache is read offline; no dependencies are installed.
module JavaPeerActors
  module_function
  def command(core)
    return [go] if core == "go"
    abort "Unknown frozen process peer #{core}" unless %w[rust node].include?(core)
    revision = JavaNativeTasks::BASELINE.fetch("semantic_revision")
    root = File.join(JavaNativeTasks::DEV, "peers", revision)
    FileUtils.mkdir_p(File.dirname(root))
    File.open("#{root}.lock", File::RDWR | File::CREAT, 0o600) do |lock|
      lock.flock(File::LOCK_EX)
      marker = File.join(root, ".#{core}-complete")
      unless File.file?(marker)
        FileUtils.mkdir_p(root)
        paths = core == "rust" ? %w[impl/rust spec] : %w[impl/ts spec]
        statuses = Open3.pipeline(["git", "archive", revision, *paths, { chdir: JavaNativeTasks::ROOT }], ["tar", "-x", "-C", root])
        abort "Cannot export frozen #{core} process peer" unless statuses.all?(&:success?)
        File.write(marker, "#{revision}\n")
      end
      if core == "rust"
        directory = File.join(root, "impl/rust")
        target = File.join(JavaNativeTasks::DEV, "process-rust-target")
        actor = File.join(target, "release/process_actor")
        abort "Offline pinned Rust process build failed" unless system({ "CARGO_TARGET_DIR" => target }, "cargo", "build", "--offline", "--locked", "--release", "--bin", "process_actor", chdir: directory)
        command = [actor]
        version, status = Open3.capture2("rustc", "--version")
        files = [actor, File.join(directory, "Cargo.lock")]
      else
        directory = File.join(root, "impl/ts")
        installed = File.join(JavaNativeTasks::ROOT, "impl/ts")
        abort "Installed Node lock differs from frozen peer" unless File.binread(File.join(directory, "package-lock.json")) == File.binread(File.join(installed, "package-lock.json"))
        modules = File.join(installed, "node_modules")
        abort "Existing approved Node dependencies are unavailable" unless File.directory?(modules)
        File.symlink(modules, File.join(directory, "node_modules")) unless File.exist?(File.join(directory, "node_modules"))
        helper = File.join(directory, "jed_lock.node")
        unless File.file?(helper)
          target = File.join(JavaNativeTasks::DEV, "process-node-lock-target")
          abort "Offline pinned Node lock helper build failed" unless system({ "CARGO_TARGET_DIR" => target }, "cargo", "build", "--offline", "--locked", "--release", chdir: File.join(directory, "native-lock"))
          FileUtils.cp(File.join(target, "release/libjed_lock.so"), helper)
        end
        command = ["node", File.join(directory, "src/bin/process_actor.ts")]
        version, status = Open3.capture2("node", "--version")
        files = [helper, File.join(directory, "package-lock.json"), *Dir.glob(File.join(directory, "src/**/*")).select { File.file?(_1) }]
      end
      abort "Cannot identify #{core} toolchain" unless status.success?
      identity = { semantic_revision: revision, core: core, toolchain: version.strip, command: command,
                   artifacts: files.sort.to_h { [_1, Digest::SHA256.file(_1).hexdigest] } }
      File.write(File.join(root, "process-#{core}.json"), JSON.pretty_generate(identity) + "\n")
      command
    end
  end

  def go(kind = :process)
    abort "Unknown peer actor #{kind}" unless %i[process simulation conformance].include?(kind)
    revision = JavaNativeTasks::BASELINE.fetch("semantic_revision")
    root = File.join(JavaNativeTasks::DEV, "peers", revision)
    FileUtils.mkdir_p(File.dirname(root))
    File.open("#{root}.lock", File::RDWR | File::CREAT, 0o600) do |lock|
      lock.flock(File::LOCK_EX)
      marker = File.join(root, ".complete")
      unless File.file?(marker)
        FileUtils.mkdir_p(root)
        statuses = Open3.pipeline(["git", "archive", revision, "impl/go", "spec", { chdir: JavaNativeTasks::ROOT }], ["tar", "-x", "-C", root])
        abort "Cannot export frozen Go process peer" unless statuses.all?(&:success?)
        File.write(marker, "#{revision}\n")
      end
      actor = File.join(root, { process: "process_actor", simulation: "sim.test", conformance: "conformance" }.fetch(kind))
      unless File.executable?(actor)
        cache = File.join(JavaNativeTasks::DEV, "go-cache")
        FileUtils.mkdir_p(cache)
        env = { "GOPROXY" => "off", "GOSUMDB" => "off", "GOTOOLCHAIN" => "local", "GOCACHE" => cache }
        command = kind == :simulation ? ["go", "test", "-c", "-trimpath", "-o", actor, "."] : ["go", "build", "-trimpath", "-o", actor, "./cmd/#{kind == :process ? 'process_actor' : 'conformance'}"]
        abort "Offline pinned Go #{kind} actor build failed" unless system(env, *command, chdir: File.join(root, "impl/go"))
        version, status = Open3.capture2("go", "version")
        abort "Cannot identify Go toolchain" unless status.success?
        File.write("#{actor}.json", JSON.pretty_generate({ semantic_revision: revision, toolchain: version.strip, sha256: Digest::SHA256.file(actor).hexdigest }) + "\n")
      end
      actor
    end
  end
end
