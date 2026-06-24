# frozen_string_literal: true

module Jed
  VERSION = "0.1.0"

  # The native C-ABI version this gem speaks. Must equal `jed_abi_version()` in the loaded
  # cdylib; the FFI loader refuses a mismatch (spec/design/ruby.md §5). v2 added `$N` bind params;
  # v3 added the decimal/date/timestamp param tags; v4 added the host-bundle loaders.
  ABI_VERSION = 4
end
