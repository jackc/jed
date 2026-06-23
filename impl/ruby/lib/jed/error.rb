# frozen_string_literal: true

module Jed
  # A structured engine error (spec/design/ruby.md §3). `sqlstate` is the canonical 5-char SQLSTATE
  # (spec/errors/registry.toml); `message` is the engine's deterministic message text. The same
  # error a Rust/Go/TS host would see — the gem only relays it.
  class Error < StandardError
    # The 5-char SQLSTATE, e.g. "23505" (unique_violation) or "42601" (syntax_error).
    attr_reader :sqlstate

    def initialize(sqlstate, message)
      @sqlstate = sqlstate
      super("#{sqlstate}: #{message}")
    end
  end

  # The native library could not be located or loaded, or it speaks a different ABI version than
  # this gem (spec/design/ruby.md §5). Distinct from {Error} (an engine error) — this is a wiring
  # problem, not a SQL one.
  class LoadError < StandardError; end
end
