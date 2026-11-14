# frozen_string_literal: true

require "digest"
require "fileutils"
require "json"
require "open3"
require "tmpdir"

module JavaNativeTasks
  ROOT = File.expand_path("../../..", __dir__)
  PACKAGE = File.join(ROOT, "impl/java-native")
  DEV = File.join(ROOT, ".dev/java-native")
  BUILD = File.join(DEV, "build")
  BASELINE = JSON.parse(File.read(File.join(PACKAGE, "baseline.json"))).freeze
  TOOLCHAINS = JSON.parse(File.read(File.join(PACKAGE, "toolchains.json"))).freeze
  VARIANTS = %w[stable ea valhalla].freeze

  module_function

  def artifact(name) = TOOLCHAINS.fetch("artifacts").fetch(name)
  def artifact_path(name)
    item = artifact(name)
    File.join(DEV, item.fetch("kind") == "jdk" ? "jdks" : "dependencies", item["directory"] || item.fetch("filename"))
  end

  def tool(name, variant = "stable")
    abort "Unknown variant #{variant}" unless VARIANTS.include?(variant)
    path = File.join(artifact_path(variant == "stable" ? "stable" : "ea"), "bin", name)
    abort "Run mise run java-native:setup first (missing #{path})" unless File.executable?(path)
    path
  end

  def dependency
    path = artifact_path("bcprov")
    abort "Run mise run java-native:setup first (missing or modified Bouncy Castle JAR)" unless
      File.file?(path) && Digest::SHA256.file(path).hexdigest == artifact("bcprov").fetch("sha256")
    path
  end

  def capture_sources(destination, root: ROOT)
    paths = Dir.glob("impl/java-native/{src,Tools}/**/*", base: root).select { File.file?(File.join(root, _1)) } +
      Dir.glob("mise-tasks/java-native/*", base: root) + %w[impl/java-native/baseline.json impl/java-native/toolchains.json]
    digest = Digest::SHA256.new
    paths.sort.each do |path|
      bytes = File.binread(File.join(root, path))
      target = File.join(destination, path)
      FileUtils.mkdir_p(File.dirname(target))
      File.binwrite(target, bytes)
      digest.update(path).update("\0").update(bytes).update("\0")
    end
    digest.hexdigest
  end

  def command!(*args)
    return if system(*args, chdir: ROOT)
    abort "Command failed (#{$?&.exitstatus}): #{args.first}"
  end

  def build(variant = "stable", source_root: ROOT)
    javac = tool("javac", variant)
    jar = tool("jar", variant)
    dependency_jar = dependency
    source_hash = nil
    output = nil
    FileUtils.mkdir_p(BUILD)
    Dir.mktmpdir("compile-", BUILD) do |temporary|
      source_hash = capture_sources(File.join(temporary, "sources"), root: source_root)
      package = File.join(temporary, "sources/impl/java-native")
      main = Dir.glob(File.join(package, "src/main/java/**/*.java")).sort
      tests = Dir.glob(File.join(package, "src/test/java/**/*.java")).sort
      if variant == "valhalla"
        replacements = Dir.glob(File.join(package, "src/valhalla/java/**/*.java")).sort
        abort "Valhalla variant has no explicit value-class sources yet" if replacements.empty?
        replacements.each do |path|
          relative = path.delete_prefix(File.join(package, "src/valhalla/java/"))
          ordinary = File.read(File.join(package, "src/main/java", relative))
          abort "Value variant changes engine behavior: #{relative}" unless File.read(path).sub(/public (final )?value (class|record)/) { "public #{$1}#{$2}" } == ordinary
          main.delete(File.join(package, "src/main/java", relative))
          main << path
        end
      end
      abort "No Java engine sources yet" if main.empty?
      options = variant == "valhalla" ? ["--release", "28", "--enable-preview"] : ["--release", "25"]
      classes = File.join(temporary, "classes")
      test_classes = File.join(temporary, "test-classes")
      FileUtils.mkdir_p([classes, test_classes])
      command!(javac, *options, "-encoding", "UTF-8", "-Xlint:all", "-d", classes, "-cp", dependency_jar, *main)
      unless tests.empty?
        command!(javac, *options, "-encoding", "UTF-8", "-Xlint:all", "-d", test_classes, "-cp", [classes, dependency_jar].join(File::PATH_SEPARATOR), *tests)
      end
      resources = File.join(package, "src/test/resources")
      FileUtils.cp_r(Dir.glob(File.join(resources, "*")), test_classes) if File.directory?(resources)
      command!(jar, "--create", "--file", File.join(temporary, "jed.jar"), "--date=2026-11-14T00:00:00Z", "-C", classes, ".")
      runtime, status = Open3.capture2e(tool("java", variant), "--version")
      abort "Cannot inspect Java runtime" unless status.success?
      File.write(File.join(temporary, "build.json"), JSON.pretty_generate(
        source_sha256: source_hash, source_snapshot: "sources", variant: variant, runtime: runtime.strip,
        release: variant == "valhalla" ? 28 : 25, preview: variant == "valhalla",
        dependency_sha256: artifact("bcprov").fetch("sha256"),
        jar_sha256: Digest::SHA256.file(File.join(temporary, "jed.jar")).hexdigest
      ) + "\n")
      output = File.join(BUILD, "#{variant}-#{File.basename(temporary)}")
      FileUtils.mv(temporary, output)
      FileUtils.mkdir_p(temporary)
    end
    puts "Built Java #{variant}: #{output} (source #{source_hash})"
    output
  end

  def capture(directory, variant)
    # Preserve the actual actor bytes even when another opt-in run rebuilds.
    captures = File.join(DEV, "actors")
    FileUtils.mkdir_p(captures)
    capture = Dir.mktmpdir("#{variant}-", captures)
    paths = Dir.glob(File.join(directory, "**/*")).select { File.file?(_1) }
    hashes = paths.to_h { |path| [path.delete_prefix(directory + "/"), Digest::SHA256.file(path).hexdigest] }
    hashes.each_key do |relative|
      target = File.join(capture, relative)
      FileUtils.mkdir_p(File.dirname(target))
      FileUtils.cp(File.join(directory, relative), target)
    end
    hashes.each do |relative, hash|
      abort "Java actor changed during capture" unless
        Digest::SHA256.file(File.join(directory, relative)).hexdigest == hash &&
        Digest::SHA256.file(File.join(capture, relative)).hexdigest == hash
    end
    dependency_copy = File.join(capture, artifact("bcprov").fetch("filename"))
    FileUtils.cp(dependency, dependency_copy)
    abort "Dependency changed during capture" unless Digest::SHA256.file(dependency_copy).hexdigest == artifact("bcprov").fetch("sha256")
    hashes[File.basename(dependency_copy)] = artifact("bcprov").fetch("sha256")
    File.write(File.join(capture, "artifacts.json"), JSON.pretty_generate(hashes) + "\n")
    puts "Captured Java actor: #{capture}"
    $stdout.flush
    capture
  end

  def actor_command(capture, variant)
    dependency_copy = File.join(capture, artifact("bcprov").fetch("filename"))
    options = ["--enable-native-access=ALL-UNNAMED", "-XX:-UsePerfData", "-ea", "-Dfile.encoding=UTF-8"]
    options << "--enable-preview" if variant == "valhalla"
    [tool("java", variant), *options, "-cp",
      [File.join(capture, "jed.jar"), File.join(capture, "test-classes"), dependency_copy].join(File::PATH_SEPARATOR), "org.jed.Check"]
  end

  def run_actor(directory, variant, *arguments)
    directory = File.expand_path(directory)
    metadata = JSON.parse(File.read(File.join(directory, "build.json")))
    abort "Actor runtime variant mismatch" unless metadata.fetch("variant") == variant
    JSON.parse(File.read(File.join(directory, "artifacts.json"))).each do |name, hash|
      abort "Immutable Java actor differs: #{name}" unless Digest::SHA256.file(File.join(directory, name)).hexdigest == hash
    end
    warn "Reusing immutable Java actor: #{directory}; source #{metadata.fetch('source_sha256')}"
    exec(*actor_command(directory, variant), *arguments, chdir: ROOT)
  end

  def run(directory, variant, *arguments)
    exec(*actor_command(capture(directory, variant), variant), *arguments, chdir: ROOT)
  end

  # Immutable local Git objects pin the acceptance target without fetching.
  def baseline_spec
    revision = BASELINE.fetch("semantic_revision")
    destination = File.join(DEV, "baseline-#{revision}")
    marker = File.join(destination, ".complete")
    return File.join(destination, "spec") if File.file?(marker) && File.read(marker).strip == revision
    FileUtils.mkdir_p(DEV)
    Dir.mktmpdir("baseline-", DEV) do |temporary|
      statuses = Open3.pipeline(["git", "archive", revision, "spec", { chdir: ROOT }], ["tar", "-x", "-C", temporary])
      abort "Cannot extract pinned spec #{revision}" unless statuses.all?(&:success?)
      File.write(File.join(temporary, ".complete"), revision + "\n")
      FileUtils.rm_rf(destination)
      FileUtils.mv(temporary, destination)
      FileUtils.mkdir_p(temporary)
    end
    File.join(destination, "spec")
  end
end
