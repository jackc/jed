# Ruby build/test tooling for the engine (see CLAUDE.md §10: prefer Ruby + Rake).
#
# Everything here is DEVELOPMENT/BUILD-TIME tooling only — there is no Ruby engine.
# In particular toml-rb parses the canonical spec data tables (types, functions,
# errors, encoding fixtures) for verification and, later, codegen. TOML is never a
# runtime dependency of any shipped core (CLAUDE.md §5).

source "https://rubygems.org"

gem "rake"        # task runner (references:*, spec verification)
gem "toml-rb"     # parser for the canonical spec data tables (TOML)
