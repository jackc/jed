# frozen_string_literal: true

# The experiment's explicit task launcher. No default repository task imports it.
require "fileutils"
require "open3"

module SwiftNativeTasks
  ROOT = File.expand_path("../../..", __dir__)
  PACKAGE = File.join(ROOT, "impl/swift-native")
  BUILD = File.join(PACKAGE, ".build")

  module_function

  def command(operation, configuration: nil, extra: [])
    unless configuration.nil? || %w[debug release].include?(configuration)
      abort "configuration must be debug or release"
    end
    %w[cache config security clang-modules swift-modules].each do |name|
      FileUtils.mkdir_p(File.join(BUILD, name))
    end
    env = {
      "CLANG_MODULE_CACHE_PATH" => File.join(BUILD, "clang-modules"),
      "SWIFTPM_MODULECACHE_OVERRIDE" => File.join(BUILD, "swift-modules")
    }
    command = ["swift", operation, "--package-path", PACKAGE,
               "--cache-path", File.join(BUILD, "cache"),
               "--config-path", File.join(BUILD, "config"),
               "--security-path", File.join(BUILD, "security"),
               "--scratch-path", BUILD, "--manifest-cache", "local"]
    command.concat(["--configuration", configuration]) if configuration
    # The compiler-driven build system needs no Swift Build service IPC; this
    # also makes sandboxed Linux builds independent of local socket permission.
    command.concat(["--build-system", "native"]) if %w[build test run].include?(operation)
    command.concat(extra)
    [env, command]
  end

  def run(operation, configuration: nil, extra: [])
    env, arguments = command(operation, configuration: configuration, extra: extra)
    Dir.chdir(PACKAGE) { exec(env, *arguments) }
  end

  # Build in the package so SwiftPM bookkeeping stays local, then execute the
  # tool in the caller's directory so source/output paths have normal semantics.
  def run_product(product, arguments:, configuration: "debug")
    env, build = command("build", configuration: configuration, extra: ["--product", product])
    unless system(env, *build, chdir: PACKAGE)
      exit($?&.exitstatus || 1)
    end
    env, location = command("build", configuration: configuration, extra: ["--show-bin-path"])
    output, error, status = Open3.capture3(env, *location, chdir: PACKAGE)
    abort error unless status.success?
    directory = output.strip
    abort "SwiftPM returned an invalid executable directory" if directory.empty? || directory.include?("\n")
    exec(env, File.join(directory, product), *arguments)
  end
end
