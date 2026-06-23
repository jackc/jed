# frozen_string_literal: true

require "fiddle"
require "rbconfig"

module Jed
  # The thin Fiddle binding to the native cdylib (spec/design/ruby.md §5). Loads `libjed_ruby`,
  # binds the eight C-ABI entry points, and verifies the ABI version on load. Uses only Ruby's
  # stdlib `fiddle` — no third-party gem (CLAUDE.md §14).
  module FFI
    module_function

    # The platform cdylib filename for the current host.
    def lib_name
      case RbConfig::CONFIG["host_os"]
      when /darwin/ then "libjed_ruby.dylib"
      when /mswin|mingw|cygwin/ then "jed_ruby.dll"
      else "libjed_ruby.so"
      end
    end

    # Resolve the cdylib path. Honors `JED_RUBY_LIB` (an explicit override, e.g. a packaged or
    # vendored artifact), then the in-repo cargo build outputs, then the gem's own lib dir (where a
    # packaged gem would stage the artifact). Raises a clear {Jed::LoadError} pointing at
    # `rake ruby:build` if none is found.
    def lib_path
      override = ENV["JED_RUBY_LIB"]
      return override if override && File.exist?(override)

      name = lib_name
      candidates = search_roots.map { |r| File.join(r, name) }
      found = candidates.find { |p| File.exist?(p) }
      return found if found

      raise Jed::LoadError, <<~MSG.strip
        could not find the native library #{name}.
        Build it with `rake ruby:build` (from the repo root) or set JED_RUBY_LIB to its path.
        Searched:
          #{candidates.join("\n  ")}
      MSG
    end

    # Directories searched for the cdylib, in priority order.
    def search_roots
      gem_root = File.expand_path("../..", __dir__) # impl/ruby
      [
        File.join(gem_root, "ext", "target", "release"),
        File.join(gem_root, "ext", "target", "debug"),
        File.join(gem_root, "lib", "jed"), # a packaged gem would stage the artifact here
        File.join(gem_root, "lib"),
      ]
    end

    LIB = Fiddle.dlopen(lib_path)

    # Fiddle type aliases used by the bindings below.
    VOIDP = Fiddle::TYPE_VOIDP
    INT = Fiddle::TYPE_INT
    UINT = Fiddle::TYPE_INT # the u32 ABI-version return; non-negative, fits an int
    CHAR = Fiddle::TYPE_CHAR # the read_only u8 flag
    VOID = Fiddle::TYPE_VOID

    def fn(sym, args, ret)
      Fiddle::Function.new(LIB[sym.to_s], args, ret, name: sym.to_s)
    end
    module_function :fn

    ABI_VERSION_FN = fn(:jed_abi_version, [], UINT)
    OPEN_MEMORY    = fn(:jed_open_memory, [], VOIDP)
    CREATE         = fn(:jed_create, [VOIDP], VOIDP)
    OPEN           = fn(:jed_open, [VOIDP, CHAR], VOIDP)
    EXECUTE        = fn(:jed_execute, [VOIDP, VOIDP], VOIDP)
    COMMIT         = fn(:jed_commit, [VOIDP], VOIDP)
    CLOSE          = fn(:jed_close, [VOIDP], VOID)
    FREE           = fn(:jed_free, [VOIDP], VOID)

    # Fail loudly if the loaded cdylib was built against a different ABI than this gem expects —
    # a stale artifact next to a newer gem (or vice versa). Better a clear error than a wire
    # misparse (spec/design/ruby.md §5).
    loaded_abi = ABI_VERSION_FN.call
    unless loaded_abi == Jed::ABI_VERSION
      raise Jed::LoadError,
        "jed native ABI mismatch: gem expects #{Jed::ABI_VERSION}, library reports #{loaded_abi} " \
        "(rebuild with `rake ruby:build`)"
    end
  end
end
