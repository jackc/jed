# frozen_string_literal: true

require_relative "tasks"

# Independent supported engines are test peers only. Their sources come from the
# immutable semantic baseline; this helper never downloads or updates packages.
module CSharpPeerActors
  module_function

  def root
    revision = CSharpNativeTasks::BASELINE.fetch("semantic_revision")
    directory = File.join(CSharpNativeTasks::BUILD, "process-peers-#{revision}")
    marker = File.join(directory, ".complete")
    unless File.file?(marker)
      FileUtils.mkdir_p(directory)
      statuses = Open3.pipeline(["git", "archive", revision, "impl/go", "impl/rust", "impl/ts", "spec", { chdir: CSharpNativeTasks::ROOT }],
                                ["tar", "-x", "-C", directory])
      abort "Cannot extract pinned process peers" unless statuses.all?(&:success?)
      File.write(marker, "#{revision}\n")
    end
    directory
  end

  def command(core)
    directory = root
    case core
    when "go"
      actor = File.join(directory, "impl/go/process_actor")
      unless File.executable?(actor)
        abort "Offline pinned Go process build failed" unless system({ "GOPROXY" => "off" }, "go", "build", "-o", actor, "./cmd/process_actor", chdir: File.join(directory, "impl/go"))
      end
      [actor]
    when "rust"
      actor = File.join(directory, "impl/rust/target/release/process_actor")
      unless File.executable?(actor)
        abort "Offline pinned Rust process build failed" unless system("cargo", "build", "--release", "--locked", "--offline", "--bin", "process_actor", chdir: File.join(directory, "impl/rust"))
      end
      [actor]
    when "node"
      typescript = File.join(directory, "impl/ts")
      installed = File.join(CSharpNativeTasks::ROOT, "impl/ts")
      abort "Installed Node package lock differs from pinned peer" unless File.binread(File.join(typescript, "package-lock.json")) == File.binread(File.join(installed, "package-lock.json"))
      abort "Existing approved Node dependencies are unavailable" unless File.directory?(File.join(installed, "node_modules"))
      modules = File.join(typescript, "node_modules")
      File.symlink(File.join(installed, "node_modules"), modules) unless File.exist?(modules)
      helper = File.join(typescript, "jed_lock.node")
      unless File.file?(helper)
        abort "Offline pinned Node lock helper build failed" unless system("cargo", "build", "--release", "--locked", "--offline", chdir: File.join(typescript, "native-lock"))
        FileUtils.cp(File.join(typescript, "native-lock/target/release/libjed_lock.so"), helper)
      end
      ["node", File.join(typescript, "src/bin/process_actor.ts")]
    else abort "Unknown pinned process peer: #{core}"
    end
  end
end
