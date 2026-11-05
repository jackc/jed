# frozen_string_literal: true

# The experiment's explicit task launcher. No default repository task imports it.
require "fileutils"

module SwiftNativeTasks
  ROOT = File.expand_path("../../..", __dir__)
  PACKAGE = File.join(ROOT, "impl/swift-native")
  BUILD = File.join(PACKAGE, ".build")

  module_function

  def run(operation, configuration: nil, extra: [])
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
    command.concat(["--build-system", "native"]) if %w[build test].include?(operation)
    command.concat(extra)
    Dir.chdir(PACKAGE) { exec(env, *command) }
  end
end
