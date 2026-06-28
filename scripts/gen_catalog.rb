#!/usr/bin/env ruby
# frozen_string_literal: true

# Codegen "middle path" (CLAUDE.md §5): generate per-language operator DESCRIPTOR
# tables from the canonical spec/functions/catalog.toml. This emits DATA only — the
# parser, executor, and the eq3/lt3 evaluation logic that CONSUME the descriptors are
# hand-written (§5 forbids codegenning those). Build-time only; no shipped core runs
# Ruby or parses TOML.
#
#   ruby scripts/gen_catalog.rb            # (re)write the generated files
#   ruby scripts/gen_catalog.rb --check    # fail if any checked-in file is stale
#
# The reasoning lives in spec/design/codegen.md — read it before editing this file.

require "bundler/setup"
require "toml-rb"

REPO = File.expand_path("..", __dir__)
CATALOG = File.join(REPO, "spec", "functions", "catalog.toml")
RANGES_SRC = File.join(REPO, "spec", "types", "ranges.toml")

# (relative path, builder method) for each generated file.
TARGETS = [
  ["impl/rust/src/operators.rs", :rust_file],
  ["impl/go/operators.go",       :go_file],
  ["impl/ts/src/operators.ts",   :ts_file],
].freeze

# (relative path, builder method) for the range-type descriptor table, generated from the
# SEPARATE spec/types/ranges.toml (the six built-in range types as data — CLAUDE.md §4/§5).
# Its builders take only the range list (not ops/aggs/srfs).
RANGE_TARGETS = [
  ["impl/rust/src/ranges_gen.rs", :rust_ranges_file],
  ["impl/go/ranges_gen.go",       :go_ranges_file],
  ["impl/ts/src/ranges_gen.ts",   :ts_ranges_file],
].freeze

def fail!(msg)
  warn "FAIL: #{msg}"
  exit 1
end

# --- shared field access -----------------------------------------------------

def operators
  catalog = TomlRB.load_file(CATALOG)
  ops = catalog["operator"] || []
  fail!("#{CATALOG}: no [[operator]] entries") if ops.empty?
  ops
end

# Aggregate functions (kind = "aggregate") — a separate descriptor table emitted
# alongside the operators. Aggregates are not operators (no symbol/precedence), so they
# get their own struct/field set. May be empty (the engine had none before this slice).
def aggregates
  catalog = TomlRB.load_file(CATALOG)
  catalog["aggregate"] || []
end

# Set-returning functions (kind = "set_returning") — a separate descriptor table again.
# An SRF expands its args into a row SET (generate_series), so it fits neither the operator
# nor the aggregate mold; it carries its own field set (surface/arity/arg_families/result/
# column/null/errors). May be empty.
def set_returning
  catalog = TomlRB.load_file(CATALOG)
  catalog["set_returning"] || []
end

# Window functions (kind = "window") — a separate descriptor table again. A window function is
# per-row AND a fold over a frame (../design/window.md); it fits neither the operator, aggregate,
# nor SRF mold, so it carries its own field set (name/surface/args/arg_families/result/
# frame_sensitive/requires_order/null/errors). May be empty.
def windows
  catalog = TomlRB.load_file(CATALOG)
  catalog["window"] || []
end

# Range types (the [[range]] array of spec/types/ranges.toml) — the six built-in range
# types as data. A STRUCTURAL container over a scalar element; the codec/comparator/text-I/O
# stay hand-written per core (CLAUDE.md §5), only this type-set table is shared.
def ranges
  data = TomlRB.load_file(RANGES_SRC)
  rs = data["range"] || []
  fail!("#{RANGES_SRC}: no [[range]] entries") if rs.empty?
  rs
end

# --- Rust --------------------------------------------------------------------

def rust_str(s) = %("#{s}")
def rust_slice(arr) = arr.empty? ? "&[]" : "&[#{arr.map { |x| rust_str(x) }.join(', ')}]"

def rust_entry(op)
  sym = op["symbol"] ? "Some(#{rust_str(op['symbol'])})" : "None"
  [
    "    OperatorDesc {",
    "        name: #{rust_str(op['name'])},",
    "        symbol: #{sym},",
    "        kind: #{rust_str(op['kind'])},",
    "        arity: #{op['arity']},",
    "        arg_families: #{rust_slice(op['arg_families'])},",
    "        arg_resolution: #{rust_str(op['arg_resolution'])},",
    "        result: #{rust_str(op['result'])},",
    "        null: #{rust_str(op['null'])},",
    "        precedence: #{op['precedence'] || 0},",
    "        errors: #{rust_slice(op['errors'])},",
    "        arg_names: #{rust_slice(op['arg_names'] || [])},",
    "        arg_defaults: #{rust_slice(op['arg_defaults'] || [])},",
    "        volatility: #{rust_str(op['volatility'] || 'immutable')},",
    "        variadic: #{op['variadic'] ? 'true' : 'false'},",
    "        cost: #{op['cost'] || 0},",
    "    },",
  ].join("\n")
end

# A Go/TS boolean literal for an optional catalog flag (absent ⇒ false).
def variadic_bool(op) = op["variadic"] ? "true" : "false"

def rust_agg_entry(ag)
  [
    "    AggregateDesc {",
    "        name: #{rust_str(ag['name'])},",
    "        surface: #{rust_str(ag['surface'])},",
    "        arg: #{rust_str(ag['arg'])},",
    "        arg_families: #{rust_slice(ag['arg_families'] || [])},",
    "        result: #{rust_str(ag['result'])},",
    "        null: #{rust_str(ag['null'])},",
    "        errors: #{rust_slice(ag['errors'])},",
    "    },",
  ].join("\n")
end

def rust_srf_entry(sf)
  [
    "    SetReturningDesc {",
    "        name: #{rust_str(sf['name'])},",
    "        surface: #{rust_str(sf['surface'])},",
    "        arity: #{sf['arity']},",
    "        arg_families: #{rust_slice(sf['arg_families'] || [])},",
    "        arg_resolution: #{rust_str(sf['arg_resolution'])},",
    "        result: #{rust_str(sf['result'])},",
    "        column: #{rust_str(sf['column'])},",
    "        null: #{rust_str(sf['null'])},",
    "        errors: #{rust_slice(sf['errors'])},",
    "    },",
  ].join("\n")
end

def rust_window_entry(w)
  [
    "    WindowDesc {",
    "        name: #{rust_str(w['name'])},",
    "        surface: #{rust_str(w['surface'])},",
    "        args: #{rust_str(w['args'])},",
    "        arg_families: #{rust_slice(w['arg_families'] || [])},",
    "        result: #{rust_str(w['result'])},",
    "        frame_sensitive: #{w['frame_sensitive'] ? 'true' : 'false'},",
    "        requires_order: #{w['requires_order'] ? 'true' : 'false'},",
    "        null: #{rust_str(w['null'])},",
    "        errors: #{rust_slice(w['errors'])},",
    "    },",
  ].join("\n")
end

def rust_file(ops, aggs, srfs, wins)
  <<~RS
    // @generated by scripts/gen_catalog.rb from spec/functions/catalog.toml — DO NOT EDIT.
    //
    // Operator + aggregate descriptor tables (CLAUDE.md §5: the codegen "middle path").
    // This is DATA only — the parser, executor, and the eq3/lt3 evaluation logic that
    // CONSUME it are hand-written (§5 forbids codegenning those). Regenerate with
    // `rake codegen`; `rake verify` fails if this file is stale. Reasoning:
    // ../../../spec/design/codegen.md.

    /// One operator's metadata, mirroring a `[[operator]]` entry in catalog.toml.
    pub struct OperatorDesc {
        pub name: &'static str,
        pub symbol: Option<&'static str>,
        pub kind: &'static str,
        pub arity: u8,
        pub arg_families: &'static [&'static str],
        pub arg_resolution: &'static str,
        pub result: &'static str,
        pub null: &'static str,
        pub precedence: u8,
        pub errors: &'static [&'static str],
        /// Parameter names for PostgreSQL named notation (functions.md §11); empty = none.
        pub arg_names: &'static [&'static str],
        /// Integer-literal DEFAULTs for the trailing parameters; empty = none.
        pub arg_defaults: &'static [&'static str],
        /// Value-stability class (functions.md §12): "immutable" | "stable" | "volatile".
        /// Default "immutable"; marks a call non-foldable (advisory today).
        pub volatility: &'static str,
        /// VARIADIC flag (array-functions.md §12): the last parameter collects a spread of
        /// trailing args, or a single array via the VARIADIC keyword. Default false.
        pub variadic: bool,
        /// Per-operator evaluation cost base (functions.md §8): the weight this operator charges
        /// in place of `operator_eval`. 0 = absent ⇒ use the uniform `operator_eval`. Size-scaled
        /// cost (decimal_work / varlen_compare / …) is separate from this static base.
        pub cost: i64,
    }

    /// Every operator in the catalog, in catalog order.
    #[rustfmt::skip]
    pub const OPERATORS: &[OperatorDesc] = &[
    #{ops.map { |op| rust_entry(op) }.join("\n")}
    ];

    /// One aggregate function's metadata, mirroring an `[[aggregate]]` entry in
    /// catalog.toml. Aggregates are not operators (no symbol/precedence/arg_resolution);
    /// `arg` is "star" (COUNT(*)) or "expr"; `result` is a scalar id or a reserved
    /// widening id (sum_widen | same_as_input). See spec/design/aggregates.md.
    pub struct AggregateDesc {
        pub name: &'static str,
        pub surface: &'static str,
        pub arg: &'static str,
        pub arg_families: &'static [&'static str],
        pub result: &'static str,
        pub null: &'static str,
        pub errors: &'static [&'static str],
    }

    /// Every aggregate in the catalog, in catalog order.
    #[rustfmt::skip]
    pub const AGGREGATES: &[AggregateDesc] = &[
    #{aggs.map { |ag| rust_agg_entry(ag) }.join("\n")}
    ];

    /// One set-returning function's metadata, mirroring a `[[set_returning]]` entry in
    /// catalog.toml. An SRF expands its args into a row SET (not a scalar/aggregate value);
    /// `result` is a reserved set id (set_of_promoted), `column` is the fixed output column
    /// name, and `null` = "empty_on_null" (any NULL arg → zero rows). The uniqueness key is
    /// (name, arity). See spec/design/functions.md §10.
    pub struct SetReturningDesc {
        pub name: &'static str,
        pub surface: &'static str,
        pub arity: u8,
        pub arg_families: &'static [&'static str],
        pub arg_resolution: &'static str,
        pub result: &'static str,
        pub column: &'static str,
        pub null: &'static str,
        pub errors: &'static [&'static str],
    }

    /// Every set-returning function in the catalog, in catalog order.
    #[rustfmt::skip]
    pub const SET_RETURNING: &[SetReturningDesc] = &[
    #{srfs.map { |sf| rust_srf_entry(sf) }.join("\n")}
    ];

    /// One window function's metadata, mirroring a `[[window]]` entry in catalog.toml. A window
    /// function is per-row AND a fold over a frame (spec/design/window.md); `args` is the argument
    /// shape (none | one | value_offset_default | value_n), `result` a scalar id or "same_as_input",
    /// `frame_sensitive` whether it reads the per-row frame, `requires_order` whether a window
    /// ORDER BY is mandatory (42P20). The catalog aggregates are ALSO window functions (with OVER);
    /// they are not duplicated here. Uniqueness key is `name`.
    pub struct WindowDesc {
        pub name: &'static str,
        pub surface: &'static str,
        pub args: &'static str,
        pub arg_families: &'static [&'static str],
        pub result: &'static str,
        pub frame_sensitive: bool,
        pub requires_order: bool,
        pub null: &'static str,
        pub errors: &'static [&'static str],
    }

    /// Every window-exclusive function in the catalog, in catalog order.
    #[rustfmt::skip]
    pub const WINDOWS: &[WindowDesc] = &[
    #{wins.map { |w| rust_window_entry(w) }.join("\n")}
    ];
  RS
end

def rust_range_entry(r)
  [
    "    RangeDesc {",
    "        id: #{rust_str(r['id'])},",
    "        element: #{rust_str(r['element'])},",
    "        aliases: #{rust_slice(r['aliases'] || [])},",
    "        discrete: #{r['discrete'] ? 'true' : 'false'},",
    "    },",
  ].join("\n")
end

def rust_ranges_file(ranges)
  <<~RS
    // @generated by scripts/gen_catalog.rb from spec/types/ranges.toml — DO NOT EDIT.
    //
    // Range-type descriptor table (CLAUDE.md §4/§5): the six built-in PostgreSQL range
    // types as DATA. The recursive value codec / comparator / text-I/O / canonicalize rule
    // that CONSUME this are hand-written per core (§5 forbids codegenning them; derived from
    // the element type, byte-identical by construction). Regenerate with `rake codegen`;
    // `rake verify` fails if this file is stale. Reasoning: ../../../spec/design/ranges.md.

    /// One range type's metadata, mirroring a `[[range]]` entry in spec/types/ranges.toml.
    /// `element` is a scalar id (../../../spec/types/scalars.toml) — the subtype the range is
    /// built over; `discrete` marks an integer/date subtype stored in canonical `[)` form.
    pub struct RangeDesc {
        pub id: &'static str,
        pub element: &'static str,
        pub aliases: &'static [&'static str],
        pub discrete: bool,
    }

    /// Every built-in range type, in ranges.toml order.
    #[rustfmt::skip]
    pub const RANGES: &[RangeDesc] = &[
    #{ranges.map { |r| rust_range_entry(r) }.join("\n")}
    ];
  RS
end

# --- Go ----------------------------------------------------------------------

def go_str(s) = %("#{s}")
def go_slice(arr) = arr.empty? ? "[]string{}" : "[]string{#{arr.map { |x| go_str(x) }.join(', ')}}"

def go_entry(op)
  fields = [
    ["Name",          go_str(op["name"])],
    ["Symbol",        go_str(op["symbol"] || "")],
    ["Kind",          go_str(op["kind"])],
    ["Arity",         op["arity"].to_s],
    ["ArgFamilies",   go_slice(op["arg_families"])],
    ["ArgResolution", go_str(op["arg_resolution"])],
    ["Result",        go_str(op["result"])],
    ["Null",          go_str(op["null"])],
    ["Precedence",    (op["precedence"] || 0).to_s],
    ["Errors",        go_slice(op["errors"])],
    ["ArgNames",      go_slice(op["arg_names"] || [])],
    ["ArgDefaults",   go_slice(op["arg_defaults"] || [])],
    ["Volatility",    go_str(op["volatility"] || "immutable")],
    ["Variadic",      variadic_bool(op)],
    ["Cost",          (op["cost"] || 0).to_s],
  ]
  w = fields.map { |k, _| k.length }.max
  lines = fields.map { |k, v| "\t\t#{k}:#{' ' * (w - k.length + 1)}#{v}," }
  "\t{\n#{lines.join("\n")}\n\t},"
end

def go_agg_entry(ag)
  fields = [
    ["Name",        go_str(ag["name"])],
    ["Surface",     go_str(ag["surface"])],
    ["Arg",         go_str(ag["arg"])],
    ["ArgFamilies", go_slice(ag["arg_families"] || [])],
    ["Result",      go_str(ag["result"])],
    ["Null",        go_str(ag["null"])],
    ["Errors",      go_slice(ag["errors"])],
  ]
  w = fields.map { |k, _| k.length }.max
  lines = fields.map { |k, v| "\t\t#{k}:#{' ' * (w - k.length + 1)}#{v}," }
  "\t{\n#{lines.join("\n")}\n\t},"
end

def go_srf_entry(sf)
  fields = [
    ["Name",          go_str(sf["name"])],
    ["Surface",       go_str(sf["surface"])],
    ["Arity",         sf["arity"].to_s],
    ["ArgFamilies",   go_slice(sf["arg_families"] || [])],
    ["ArgResolution", go_str(sf["arg_resolution"])],
    ["Result",        go_str(sf["result"])],
    ["Column",        go_str(sf["column"])],
    ["Null",          go_str(sf["null"])],
    ["Errors",        go_slice(sf["errors"])],
  ]
  w = fields.map { |k, _| k.length }.max
  lines = fields.map { |k, v| "\t\t#{k}:#{' ' * (w - k.length + 1)}#{v}," }
  "\t{\n#{lines.join("\n")}\n\t},"
end

def go_window_entry(w)
  fields = [
    ["Name",           go_str(w["name"])],
    ["Surface",        go_str(w["surface"])],
    ["Args",           go_str(w["args"])],
    ["ArgFamilies",    go_slice(w["arg_families"] || [])],
    ["Result",         go_str(w["result"])],
    ["FrameSensitive", w["frame_sensitive"] ? "true" : "false"],
    ["RequiresOrder",  w["requires_order"] ? "true" : "false"],
    ["Null",           go_str(w["null"])],
    ["Errors",         go_slice(w["errors"])],
  ]
  w_ = fields.map { |k, _| k.length }.max
  lines = fields.map { |k, v| "\t\t#{k}:#{' ' * (w_ - k.length + 1)}#{v}," }
  "\t{\n#{lines.join("\n")}\n\t},"
end

def go_file(ops, aggs, srfs, wins)
  <<~GO
    // Code generated by scripts/gen_catalog.rb from spec/functions/catalog.toml. DO NOT EDIT.
    //
    // Operator + aggregate descriptor tables (CLAUDE.md §5: the codegen "middle path").
    // DATA only — the parser, executor, and the Eq3/Lt3 evaluation logic that CONSUME it
    // are hand-written (§5 forbids codegenning those). Regenerate with `rake codegen`;
    // `rake verify` fails if this file is stale. Reasoning: ../../spec/design/codegen.md.

    package jed

    // OperatorDesc is one operator's metadata, mirroring a [[operator]] entry in catalog.toml.
    // Symbol is "" for operators with no infix symbol (the IS [NOT] NULL tests).
    type operatorDesc struct {
    \tName          string
    \tSymbol        string
    \tKind          string
    \tArity         int
    \tArgFamilies   []string
    \tArgResolution string
    \tResult        string
    \tNull          string
    \tPrecedence    int
    \tErrors        []string
    \t// ArgNames holds the parameter names for PostgreSQL named notation (functions.md §11);
    \t// empty = none. ArgDefaults holds integer-literal DEFAULTs for the trailing parameters.
    \tArgNames    []string
    \tArgDefaults []string
    \t// Volatility is the value-stability class (functions.md §12): "immutable" | "stable" |
    \t// "volatile". Default "immutable"; marks a call non-foldable (advisory today).
    \tVolatility string
    \t// Variadic marks the last parameter VARIADIC (array-functions.md §12): a spread of trailing
    \t// args, or a single array via the VARIADIC keyword. Default false.
    \tVariadic bool
    \t// Cost is the per-operator evaluation cost base (functions.md §8): the weight this operator
    \t// charges in place of OperatorEval. 0 = absent ⇒ use the uniform OperatorEval. Size-scaled
    \t// cost (DecimalWork / VarlenCompare / …) is separate from this static base.
    \tCost int64
    }

    // Operators lists every operator in the catalog, in catalog order.
    var operators = []operatorDesc{
    #{ops.map { |op| go_entry(op) }.join("\n")}
    }

    // AggregateDesc is one aggregate function's metadata, mirroring an [[aggregate]] entry
    // in catalog.toml. Aggregates are not operators (no symbol/precedence/arg_resolution);
    // Arg is "star" (COUNT(*)) or "expr"; Result is a scalar id or a reserved widening id
    // (sum_widen | same_as_input). See spec/design/aggregates.md.
    type aggregateDesc struct {
    \tName        string
    \tSurface     string
    \tArg         string
    \tArgFamilies []string
    \tResult      string
    \tNull        string
    \tErrors      []string
    }

    // Aggregates lists every aggregate in the catalog, in catalog order.
    var aggregates = []aggregateDesc{
    #{aggs.map { |ag| go_agg_entry(ag) }.join("\n")}
    }

    // SetReturningDesc is one set-returning function's metadata, mirroring a [[set_returning]]
    // entry in catalog.toml. An SRF expands its args into a row SET (not a scalar/aggregate
    // value); Result is a reserved set id (set_of_promoted), Column is the fixed output column
    // name, and Null = "empty_on_null" (any NULL arg -> zero rows). The uniqueness key is
    // (Name, Arity). See spec/design/functions.md §10.
    type setReturningDesc struct {
    \tName          string
    \tSurface       string
    \tArity         int
    \tArgFamilies   []string
    \tArgResolution string
    \tResult        string
    \tColumn        string
    \tNull          string
    \tErrors        []string
    }

    // SetReturning lists every set-returning function in the catalog, in catalog order.
    var setReturning = []setReturningDesc{
    #{srfs.map { |sf| go_srf_entry(sf) }.join("\n")}
    }

    // WindowDesc is one window function's metadata, mirroring a [[window]] entry in catalog.toml.
    // A window function is per-row AND a fold over a frame (spec/design/window.md); Args is the
    // argument shape (none | one | value_offset_default | value_n), Result a scalar id or
    // "same_as_input", FrameSensitive whether it reads the per-row frame, RequiresOrder whether a
    // window ORDER BY is mandatory (42P20). The catalog aggregates are ALSO window functions (with
    // OVER); they are not duplicated here. Uniqueness key is Name.
    type windowDesc struct {
    \tName           string
    \tSurface        string
    \tArgs           string
    \tArgFamilies    []string
    \tResult         string
    \tFrameSensitive bool
    \tRequiresOrder  bool
    \tNull           string
    \tErrors         []string
    }

    // Windows lists every window-exclusive function in the catalog, in catalog order.
    var windows = []windowDesc{
    #{wins.map { |w| go_window_entry(w) }.join("\n")}
    }
  GO
end

def go_range_entry(r)
  fields = [
    ["ID",       go_str(r["id"])],
    ["Element",  go_str(r["element"])],
    ["Aliases",  go_slice(r["aliases"] || [])],
    ["Discrete", r["discrete"] ? "true" : "false"],
  ]
  w = fields.map { |k, _| k.length }.max
  lines = fields.map { |k, v| "\t\t#{k}:#{' ' * (w - k.length + 1)}#{v}," }
  "\t{\n#{lines.join("\n")}\n\t},"
end

def go_ranges_file(ranges)
  <<~GO
    // Code generated by scripts/gen_catalog.rb from spec/types/ranges.toml. DO NOT EDIT.
    //
    // Range-type descriptor table (CLAUDE.md §4/§5): the six built-in PostgreSQL range types
    // as DATA. The recursive value codec / comparator / text-I/O / canonicalize rule that
    // CONSUME it are hand-written per core (§5 forbids codegenning them; derived from the
    // element type, byte-identical by construction). Regenerate with `rake codegen`; `rake
    // verify` fails if stale. Reasoning: ../../spec/design/ranges.md.

    package jed

    // RangeDesc is one range type's metadata, mirroring a [[range]] entry in
    // spec/types/ranges.toml. Element is a scalar id (the subtype the range is built over);
    // Discrete marks an integer/date subtype stored in canonical [) form.
    type rangeDesc struct {
    \tID       string
    \tElement  string
    \tAliases  []string
    \tDiscrete bool
    }

    // Ranges lists every built-in range type, in ranges.toml order.
    var ranges = []rangeDesc{
    #{ranges.map { |r| go_range_entry(r) }.join("\n")}
    }
  GO
end

# --- TypeScript --------------------------------------------------------------

def ts_str(s) = %("#{s}")
def ts_arr(arr) = "[#{arr.map { |x| ts_str(x) }.join(', ')}]"

def ts_entry(op)
  lines = ["  {"]
  lines << "    name: #{ts_str(op['name'])},"
  lines << "    symbol: #{ts_str(op['symbol'])}," if op["symbol"]
  lines << "    kind: #{ts_str(op['kind'])},"
  lines << "    arity: #{op['arity']},"
  lines << "    argFamilies: #{ts_arr(op['arg_families'])},"
  lines << "    argResolution: #{ts_str(op['arg_resolution'])},"
  lines << "    result: #{ts_str(op['result'])},"
  lines << "    null: #{ts_str(op['null'])},"
  lines << "    precedence: #{op['precedence'] || 0},"
  lines << "    errors: #{ts_arr(op['errors'])},"
  lines << "    argNames: #{ts_arr(op['arg_names'] || [])},"
  lines << "    argDefaults: #{ts_arr(op['arg_defaults'] || [])},"
  lines << "    volatility: #{ts_str(op['volatility'] || 'immutable')},"
  lines << "    variadic: #{variadic_bool(op)},"
  lines << "    cost: #{op['cost'] || 0},"
  lines << "  },"
  lines.join("\n")
end

def ts_agg_entry(ag)
  lines = ["  {"]
  lines << "    name: #{ts_str(ag['name'])},"
  lines << "    surface: #{ts_str(ag['surface'])},"
  lines << "    arg: #{ts_str(ag['arg'])},"
  lines << "    argFamilies: #{ts_arr(ag['arg_families'] || [])},"
  lines << "    result: #{ts_str(ag['result'])},"
  lines << "    null: #{ts_str(ag['null'])},"
  lines << "    errors: #{ts_arr(ag['errors'])},"
  lines << "  },"
  lines.join("\n")
end

def ts_srf_entry(sf)
  lines = ["  {"]
  lines << "    name: #{ts_str(sf['name'])},"
  lines << "    surface: #{ts_str(sf['surface'])},"
  lines << "    arity: #{sf['arity']},"
  lines << "    argFamilies: #{ts_arr(sf['arg_families'] || [])},"
  lines << "    argResolution: #{ts_str(sf['arg_resolution'])},"
  lines << "    result: #{ts_str(sf['result'])},"
  lines << "    column: #{ts_str(sf['column'])},"
  lines << "    null: #{ts_str(sf['null'])},"
  lines << "    errors: #{ts_arr(sf['errors'])},"
  lines << "  },"
  lines.join("\n")
end

def ts_window_entry(w)
  lines = ["  {"]
  lines << "    name: #{ts_str(w['name'])},"
  lines << "    surface: #{ts_str(w['surface'])},"
  lines << "    args: #{ts_str(w['args'])},"
  lines << "    argFamilies: #{ts_arr(w['arg_families'] || [])},"
  lines << "    result: #{ts_str(w['result'])},"
  lines << "    frameSensitive: #{w['frame_sensitive'] ? 'true' : 'false'},"
  lines << "    requiresOrder: #{w['requires_order'] ? 'true' : 'false'},"
  lines << "    null: #{ts_str(w['null'])},"
  lines << "    errors: #{ts_arr(w['errors'])},"
  lines << "  },"
  lines.join("\n")
end

def ts_file(ops, aggs, srfs, wins)
  <<~TS
    // @generated by scripts/gen_catalog.rb from spec/functions/catalog.toml — DO NOT EDIT.
    //
    // Operator + aggregate descriptor tables (CLAUDE.md §5: the codegen "middle path").
    // DATA only — the parser, executor, and the eq3/lt3 evaluation logic that CONSUME it
    // are hand-written (§5 forbids codegenning those). Regenerate with `rake codegen`;
    // `rake verify` fails if this file is stale. Reasoning: ../../../spec/design/codegen.md.

    // One operator's metadata, mirroring a [[operator]] entry in catalog.toml. `symbol` is
    // absent for operators with no infix symbol (the IS [NOT] NULL tests).
    export interface OperatorDesc {
      name: string;
      symbol?: string;
      kind: string;
      arity: number;
      argFamilies: readonly string[];
      argResolution: string;
      result: string;
      null: string;
      precedence: number;
      errors: readonly string[];
      // Parameter names for PostgreSQL named notation (functions.md §11); empty = none.
      argNames: readonly string[];
      // Integer-literal DEFAULTs for the trailing parameters; empty = none.
      argDefaults: readonly string[];
      // Value-stability class (functions.md §12): "immutable" | "stable" | "volatile".
      // Default "immutable"; marks a call non-foldable (advisory today).
      volatility: string;
      // VARIADIC flag (array-functions.md §12): the last parameter collects a spread of trailing
      // args, or a single array via the VARIADIC keyword. Default false.
      variadic: boolean;
      // Per-operator evaluation cost base (functions.md §8): the weight this operator charges in
      // place of operatorEval. 0 = absent ⇒ use the uniform operatorEval. Size-scaled cost
      // (decimalWork / varlenCompare / …) is separate from this static base.
      cost: number;
    }

    // Every operator in the catalog, in catalog order.
    export const OPERATORS: readonly OperatorDesc[] = [
    #{ops.map { |op| ts_entry(op) }.join("\n")}
    ];

    // One aggregate function's metadata, mirroring an [[aggregate]] entry in catalog.toml.
    // Aggregates are not operators (no symbol/precedence/argResolution); `arg` is "star"
    // (COUNT(*)) or "expr"; `result` is a scalar id or a reserved widening id (sum_widen |
    // same_as_input). See spec/design/aggregates.md.
    export interface AggregateDesc {
      name: string;
      surface: string;
      arg: string;
      argFamilies: readonly string[];
      result: string;
      null: string;
      errors: readonly string[];
    }

    // Every aggregate in the catalog, in catalog order.
    export const AGGREGATES: readonly AggregateDesc[] = [
    #{aggs.map { |ag| ts_agg_entry(ag) }.join("\n")}
    ];

    // One set-returning function's metadata, mirroring a [[set_returning]] entry in
    // catalog.toml. An SRF expands its args into a row SET (not a scalar/aggregate value);
    // `result` is a reserved set id (set_of_promoted), `column` is the fixed output column
    // name, and `null` = "empty_on_null" (any NULL arg → zero rows). The uniqueness key is
    // (name, arity). See spec/design/functions.md §10.
    export interface SetReturningDesc {
      name: string;
      surface: string;
      arity: number;
      argFamilies: readonly string[];
      argResolution: string;
      result: string;
      column: string;
      null: string;
      errors: readonly string[];
    }

    // Every set-returning function in the catalog, in catalog order.
    export const SET_RETURNING: readonly SetReturningDesc[] = [
    #{srfs.map { |sf| ts_srf_entry(sf) }.join("\n")}
    ];

    // One window function's metadata, mirroring a [[window]] entry in catalog.toml. A window
    // function is per-row AND a fold over a frame (spec/design/window.md); `args` is the argument
    // shape (none | one | value_offset_default | value_n), `result` a scalar id or "same_as_input",
    // `frameSensitive` whether it reads the per-row frame, `requiresOrder` whether a window ORDER BY
    // is mandatory (42P20). The catalog aggregates are ALSO window functions (with OVER); they are
    // not duplicated here. Uniqueness key is `name`.
    export interface WindowDesc {
      name: string;
      surface: string;
      args: string;
      argFamilies: readonly string[];
      result: string;
      frameSensitive: boolean;
      requiresOrder: boolean;
      null: string;
      errors: readonly string[];
    }

    // Every window-exclusive function in the catalog, in catalog order.
    export const WINDOWS: readonly WindowDesc[] = [
    #{wins.map { |w| ts_window_entry(w) }.join("\n")}
    ];
  TS
end

def ts_range_entry(r)
  lines = ["  {"]
  lines << "    id: #{ts_str(r['id'])},"
  lines << "    element: #{ts_str(r['element'])},"
  lines << "    aliases: #{ts_arr(r['aliases'] || [])},"
  lines << "    discrete: #{r['discrete'] ? 'true' : 'false'},"
  lines << "  },"
  lines.join("\n")
end

def ts_ranges_file(ranges)
  <<~TS
    // @generated by scripts/gen_catalog.rb from spec/types/ranges.toml — DO NOT EDIT.
    //
    // Range-type descriptor table (CLAUDE.md §4/§5): the six built-in PostgreSQL range
    // types as DATA. The recursive value codec / comparator / text-I/O / canonicalize rule
    // that CONSUME this are hand-written per core (§5 forbids codegenning them; derived from
    // the element type, byte-identical by construction). Regenerate with `rake codegen`;
    // `rake verify` fails if this file is stale. Reasoning: ../../../spec/design/ranges.md.

    // One range type's metadata, mirroring a [[range]] entry in spec/types/ranges.toml.
    // `element` is a scalar id (the subtype the range is built over); `discrete` marks an
    // integer/date subtype stored in canonical `[)` form.
    export interface RangeDesc {
      id: string;
      element: string;
      aliases: readonly string[];
      discrete: boolean;
    }

    // Every built-in range type, in ranges.toml order.
    export const RANGES: readonly RangeDesc[] = [
    #{ranges.map { |r| ts_range_entry(r) }.join("\n")}
    ];
  TS
end

# --- driver ------------------------------------------------------------------

def main
  check = ARGV.include?("--check")
  ops = operators
  aggs = aggregates
  srfs = set_returning
  wins = windows
  rngs = ranges
  stale = []

  TARGETS.each do |rel, builder|
    path = File.join(REPO, rel)
    content = send(builder, ops, aggs, srfs, wins)
    if check
      current = File.exist?(path) ? File.read(path) : nil
      stale << rel unless current == content
    else
      File.write(path, content)
      puts "wrote #{rel}"
    end
  end

  RANGE_TARGETS.each do |rel, builder|
    path = File.join(REPO, rel)
    content = send(builder, rngs)
    if check
      current = File.exist?(path) ? File.read(path) : nil
      stale << rel unless current == content
    else
      File.write(path, content)
      puts "wrote #{rel}"
    end
  end

  if check
    unless stale.empty?
      stale.each { |rel| warn "STALE: #{rel} — run 'rake codegen'" }
      exit 1
    end
    puts "OK: #{TARGETS.length + RANGE_TARGETS.length} generated files current " \
         "(#{ops.length} operators, #{aggs.length} aggregates, #{srfs.length} set-returning, " \
         "#{wins.length} window, #{rngs.length} ranges)"
  end
end

main
