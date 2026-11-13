# frozen_string_literal: true

require "fileutils"
require "json"
require "open3"
require "digest"

module CSharpNativeTasks
  ROOT = File.expand_path("../../..", __dir__)
  PACKAGE = File.join(ROOT, "impl/csharp-native")
  BUILD = File.join(PACKAGE, ".build")
  SDK = File.join(ROOT, ".dev/csharp-native/dotnet")
  CLI_HOME = File.join(ROOT, ".dev/csharp-native/cli")
  BASELINE = JSON.parse(File.read(File.join(PACKAGE, "baseline.json"))).freeze

  module_function

  def sdk_version = JSON.parse(File.read(File.join(PACKAGE, "global.json"))).fetch("sdk").fetch("version")

  def source_digest
    paths = Dir.glob("impl/csharp-native/{Jed,Jed.Check}/*.cs", base: ROOT) +
            Dir.glob("impl/csharp-native/{Jed,Jed.Check}/*.csproj", base: ROOT) +
            Dir.glob("impl/csharp-native/Tools/*.rb", base: ROOT) +
            Dir.glob("mise-tasks/csharp-native/*", base: ROOT) +
            %w[impl/csharp-native/global.json impl/csharp-native/baseline.json
               impl/csharp-native/Directory.Build.props impl/csharp-native/NuGet.Config] +
            BASELINE.fetch("supplemental_corpus")
    digest = Digest::SHA256.new
    paths.sort.each { |path| digest.update(path).update("\0").update(File.binread(File.join(ROOT, path))).update("\0") }
    digest.hexdigest
  end

  def environment
    FileUtils.mkdir_p(CLI_HOME)
    {
      "DOTNET_CLI_HOME" => CLI_HOME,
      "DOTNET_CLI_TELEMETRY_OPTOUT" => "1",
      "DOTNET_SKIP_FIRST_TIME_EXPERIENCE" => "1",
      "DOTNET_NOLOGO" => "1",
      "NUGET_PACKAGES" => File.join(BUILD, "packages")
    }.tap { |env| env["DOTNET_ROOT"] = SDK if File.executable?(File.join(SDK, "dotnet")) }
  end

  def dotnet
    local = File.join(SDK, "dotnet")
    return local if File.executable?(local)
    found = ENV.fetch("PATH").split(File::PATH_SEPARATOR).any? { |p| File.executable?(File.join(p, "dotnet")) }
    abort "Run mise run csharp-native:setup to install SDK #{sdk_version}" unless found
    "dotnet"
  end

  def run(*arguments)
    exec(environment, dotnet, *arguments, chdir: PACKAGE)
  end

  def build(configuration = "Release", runtime_async: false)
    abort "configuration must be Debug or Release" unless %w[Debug Release].include?(configuration)
    feature = runtime_async ? "runtime-async=on" : ""
    puts "C# target net11.0, SDK #{sdk_version}, #{configuration}, Runtime Async #{runtime_async ? 'on' : 'default'}"
    puts "Source SHA-256: #{source_digest}"
    $stdout.flush
    ok = system(environment, dotnet, "build", "Jed.Check/Jed.Check.csproj", "--configuration", configuration,
                "--no-incremental", "-p:Features=#{feature}",
                "--nologo", chdir: PACKAGE)
    exit($?&.exitstatus || 1) unless ok
    File.join(PACKAGE, "Jed.Check/bin", configuration, "net11.0/Jed.Check.dll")
  end

  def check(*arguments, configuration: "Release", runtime_async: false)
    dll = build(configuration, runtime_async: runtime_async)
    run(dll, *arguments)
  end

  # Use Git's immutable object store so later supported-core changes cannot move
  # the experiment's semantic baseline. This performs no network operation.
  def baseline_spec
    revision = BASELINE.fetch("semantic_revision")
    destination = File.join(BUILD, "baseline-#{revision}")
    marker = File.join(destination, ".complete")
    return File.join(destination, "spec") if File.file?(marker)
    FileUtils.mkdir_p(destination)
    statuses = Open3.pipeline(["git", "archive", revision, "spec", { chdir: ROOT }],
                              ["tar", "-x", "-C", destination])
    abort "Cannot extract pinned spec #{revision}" unless statuses.all?(&:success?)
    File.write(marker, "#{revision}\n")
    File.join(destination, "spec")
  end
end
