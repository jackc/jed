# frozen_string_literal: true

require "fiddle"
require "rbconfig"

module Jed
  # The thin Fiddle binding to the native cdylib (spec/design/ruby.md §5). Loads `libjed_ruby`,
  # binds the C-ABI entry points, and verifies the ABI version on load. Uses only Ruby's
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
    # `mise run ruby:build` if none is found.
    def lib_path
      override = ENV["JED_RUBY_LIB"]
      return override if override && File.exist?(override)

      name = lib_name
      candidates = search_roots.map { |r| File.join(r, name) }
      found = candidates.find { |p| File.exist?(p) }
      return found if found

      raise Jed::LoadError, <<~MSG.strip
        could not find the native library #{name}.
        Build it with `mise run ruby:build` (from the repo root) or set JED_RUBY_LIB to its path.
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
    # Host-function callback types (ruby.md §5b): pointers cross as integers, so a closure call builds
    # no Fiddle::Pointer.
    UINTPTR = Fiddle::TYPE_UINTPTR_T
    U64 = Fiddle::TYPE_UINT64_T
    I64 = Fiddle::TYPE_INT64_T

    def fn(sym, args, ret, need_gvl: false)
      Fiddle::Function.new(LIB[sym.to_s], args, ret, name: sym.to_s, need_gvl: need_gvl)
    end
    module_function :fn

    ABI_VERSION_FN = fn(:jed_abi_version, [], UINT)

    # Check ABI compatibility BEFORE binding the rest: a stale cdylib then fails with a clear
    # version message rather than an obscure missing-symbol error on a newer binding below
    # (spec/design/ruby.md §5).
    loaded_abi = ABI_VERSION_FN.call
    unless loaded_abi == Jed::ABI_VERSION
      raise Jed::LoadError,
        "jed native ABI mismatch: gem expects #{Jed::ABI_VERSION}, library reports #{loaded_abi} " \
        "(rebuild with `mise run ruby:build`)"
    end

    # The trailing VOIDP of the three opens is the environment (null = a fresh empty one, ruby.md §5a).
    OPEN_MEMORY    = fn(:jed_open_memory, [VOIDP], VOIDP)
    CREATE         = fn(:jed_create, [VOIDP, VOIDP], VOIDP)
    OPEN           = fn(:jed_open, [VOIDP, CHAR, VOIDP], VOIDP)
    # (db, sql, params_buf, params_len) — the param buffer is the ruby.md §3a encoding (null/0 = none).
    EXECUTE        = fn(:jed_execute, [VOIDP, VOIDP, VOIDP, UINT], VOIDP)
    COMMIT         = fn(:jed_commit, [VOIDP], VOIDP)
    CLOSE          = fn(:jed_close, [VOIDP], VOID)
    FREE           = fn(:jed_free, [VOIDP], VOID)
    # Environments (ruby.md §5a): the builder, the frozen environment, and derivation.
    ENV_BUILDER_NEW      = fn(:jed_env_builder_new, [], VOIDP)
    ENV_BUILDER_FREE     = fn(:jed_env_builder_free, [VOIDP], VOID)
    # (builder, bytes, len) → UNIT | ERROR (a malformed bundle; the builder is unchanged).
    ENV_LOAD_UNICODE     = fn(:jed_env_builder_load_unicode_data, [VOIDP, VOIDP, UINT], VOIDP)
    ENV_LOAD_TIMEZONE    = fn(:jed_env_builder_load_time_zone_data, [VOIDP, VOIDP, UINT], VOIDP)
    # (builder, name, arg_types csv, result, volatility, cost, batched, callback, user_data) → TYPES|ERROR
    ENV_REGISTER         = fn(:jed_env_builder_register,
      [VOIDP, VOIDP, VOIDP, VOIDP, CHAR, I64, CHAR, VOIDP, UINTPTR], VOIDP)
    # (builder, source or null for the default) → UNIT | ERROR (the wrong kind of source).
    ENV_RANDOM_SOURCE    = fn(:jed_env_builder_random_source, [VOIDP, VOIDP], VOIDP)
    ENV_CLOCK_SOURCE     = fn(:jed_env_builder_clock_source, [VOIDP, VOIDP], VOIDP)
    ENV_BUILD            = fn(:jed_env_builder_build, [VOIDP], VOIDP)
    ENV_TO_BUILDER       = fn(:jed_env_to_builder, [VOIDP], VOIDP)
    ENV_FREE             = fn(:jed_env_free, [VOIDP], VOID)
    # Controllable random and clock sources (ruby.md §5c).
    SEEDED_RANDOM_NEW    = fn(:jed_seeded_random_new, [U64], VOIDP)
    SEEDED_RANDOM_RESEED = fn(:jed_seeded_random_reseed, [VOIDP, U64], VOID)
    MANUAL_CLOCK_NEW     = fn(:jed_manual_clock_new, [I64], VOIDP)
    MANUAL_CLOCK_SET     = fn(:jed_manual_clock_set, [VOIDP, I64], VOID)
    MANUAL_CLOCK_ADVANCE = fn(:jed_manual_clock_advance, [VOIDP, I64], VOID)
    ADVANCING_CLOCK_NEW  = fn(:jed_advancing_clock_new, [I64, I64], VOIDP)
    ADVANCING_CLOCK_RESET = fn(:jed_advancing_clock_reset, [VOIDP, I64, I64], VOID)
    SOURCE_FREE          = fn(:jed_source_free, [VOIDP], VOID)
    # The large-result sink a host-function callback calls while it holds the GVL (ruby.md §5b;
    # need_gvl: true — a memcpy, so releasing and re-acquiring the GVL would only cost).
    HOST_RESULT       = fn(:jed_host_result, [UINTPTR, VOIDP, U64], VOID, need_gvl: true)
  end
end
