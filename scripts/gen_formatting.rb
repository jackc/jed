#!/usr/bin/env ruby
# frozen_string_literal: true

# Codegen "middle path" (CLAUDE.md §5): generate the per-language data-type formatting tables
# (PostgreSQL's DCH_keywords, DCH_suff, NUM_keywords, and the fixed word lists) from the canonical
# spec/types/formatting.toml. This emits DATA only — the template parser, the to_char and
# to_timestamp processors, and the number processor stay hand-written in each core's formatting
# module. Build-time only; no shipped core runs Ruby or parses TOML.
#
#   ruby scripts/gen_formatting.rb            # (re)write the generated files
#   ruby scripts/gen_formatting.rb --check    # fail if any checked-in file is stale
#
# Reasoning: spec/design/formatting.md and spec/design/codegen.md.

require "bundler/setup"
require "toml-rb"

REPO = File.expand_path("..", __dir__)
SRC = File.join(REPO, "spec", "types", "formatting.toml")

TARGETS = [
  ["impl/rust/src/formatting_gen.rs", :rust_file],
  ["impl/go/formatting_gen.go",       :go_file],
  ["impl/ts/src/formatting_gen.ts",   :ts_file],
].freeze

DATE_MODES = %w[none gregorian isoweek].freeze
# PostgreSQL's suffix flag bits (formatting.c DCH_S_*).
SUFFIX_BITS = { "FM" => 0x01, "TH" => 0x02, "th" => 0x04, "SP" => 0x08, "TM" => 0x10 }.freeze
WORD_LISTS = {
  "months_full" => 12, "months_abbrev" => 12, "days_full" => 7, "days_abbrev" => 7,
  "rm_months_upper" => 12, "rm_months_lower" => 12, "rn_units" => 9, "rn_tens" => 9,
  "rn_hundreds" => 9, "ampm" => 4, "ampm_long" => 4, "adbc" => 4, "adbc_long" => 4,
  "ordinal_upper" => 4, "ordinal_lower" => 4,
}.freeze

def fail!(msg)
  warn "FAIL: #{msg}"
  exit 1
end

# --- load + validate -----------------------------------------------------------

def check_keywords(table, entries)
  fail!("#{table}: no entries") if entries.nil? || entries.empty?
  seen_first = {}
  prev_first = nil
  entries.each_with_index do |e, i|
    name = e["name"]
    fail!("#{table}[#{i}]: bad name #{name.inspect}") unless name.is_a?(String) && name.match?(/\A[\x21-\x7d]+\z/)
    fail!("#{table}[#{i}]: bad id #{e['id'].inspect}") unless e["id"].is_a?(String) && e["id"].match?(/\A\w+\z/)
    first = name[0]
    # The sequential search starts at a first byte's first keyword and runs while the first byte
    # matches, so every first byte's keywords must be contiguous.
    fail!("#{table}: keywords starting with #{first.inspect} are not contiguous") if first != prev_first && seen_first[first]
    seen_first[first] = true
    prev_first = first
    next unless table == "dch"

    fail!("dch #{name}: is_digit must be a boolean") unless [true, false].include?(e["is_digit"])
    fail!("dch #{name}: bad date_mode #{e['date_mode'].inspect}") unless DATE_MODES.include?(e["date_mode"])
  end
end

def load_data
  data = TomlRB.load_file(SRC)
  fail!("schema_version must be 1") unless data["schema_version"] == 1
  check_keywords("dch", data["dch"])
  check_keywords("num", data["num"])
  data["suffixes"].each do |s|
    fail!("suffix #{s['name']}: unknown id #{s['id']}") unless SUFFIX_BITS.key?(s["id"])
    fail!("suffix #{s['name']}: bad type") unless %w[prefix postfix].include?(s["type"])
  end
  WORD_LISTS.each do |k, n|
    list = data["words"][k]
    fail!("words.#{k} must have #{n} entries") unless list.is_a?(Array) && list.length == n
  end
  data
end

# Distinct ids in first-appearance order: the action enum.
def ids(entries) = entries.map { |e| e["id"] }.uniq

# The first keyword index for each ASCII byte (-1 when no keyword starts with it).
def index_of(entries)
  idx = Array.new(128, -1)
  entries.each_with_index { |e, i| idx[e["name"].ord] = i if idx[e["name"].ord] == -1 }
  idx
end

def str_lit(s) = s.inspect

# --- Go ------------------------------------------------------------------------

def go_ident(prefix, id)
  id = { "0" => "Zero", "9" => "Nine" }.fetch(id, id)
  "#{prefix}#{id}"
end

# A gofmt-aligned `name = value` block (gofmt aligns the `=` of consecutive lines).
def go_consts(pairs)
  width = pairs.map { |n, _| n.length }.max
  pairs.map { |n, v| "\t#{n.ljust(width)} = #{v}" }.join("\n")
end

def go_file(data)
  dch_ids = go_consts(ids(data["dch"]).each_with_index.map { |id, i| [go_ident("dch", id), i.to_s] })
  num_ids = go_consts(ids(data["num"]).each_with_index.map { |id, i| [go_ident("num", id), i.to_s] })
  dch = data["dch"].map do |e|
    mode = "fmtMode#{e['date_mode'].capitalize}"
    "\t{name: #{str_lit(e['name'])}, id: #{go_ident('dch', e['id'])}, isDigit: #{e['is_digit']}, dateMode: #{mode}},"
  end.join("\n")
  num = data["num"].map { |e| "\t{name: #{str_lit(e['name'])}, id: #{go_ident('num', e['id'])}}," }.join("\n")
  suff = data["suffixes"].map do |s|
    "\t{name: #{str_lit(s['name'])}, flag: dchSuffix#{s['id']}, prefix: #{s['type'] == 'prefix'}},"
  end.join("\n")
  bits = go_consts(SUFFIX_BITS.map { |k, v| ["dchSuffix#{k}", format("0x%02x", v)] })
  words = go_consts(WORD_LISTS.keys.map do |k|
    camel = k.split("_").map(&:capitalize).join
    ["fmtWord#{camel}", "[]string{#{data['words'][k].map { |w| str_lit(w) }.join(', ')}}"]
  end)
  <<~GO
    // Code generated by scripts/gen_formatting.rb from spec/types/formatting.toml. DO NOT EDIT.
    //
    // PostgreSQL's data-type formatting tables (CLAUDE.md §5): DCH_keywords, DCH_suff, NUM_keywords,
    // and the fixed word lists, as DATA. The template parser and the processors that CONSUME them
    // are hand-written in formatting.go. Regenerate with `mise run codegen`; `mise run verify` fails
    // if stale. Reasoning: ../../spec/design/formatting.md.

    package jed

    // fmtKeyword is one template pattern. id is its action; isDigit and dateMode are the datetime
    // reader's facts (unused for number patterns).
    type fmtKeyword struct {
    \tname     string
    \tid       int
    \tisDigit  bool
    \tdateMode int
    }

    // fmtSuffix is one datetime modifier: flag is its suffix bit, prefix whether it precedes a pattern.
    type fmtSuffix struct {
    \tname   string
    \tflag   int
    \tprefix bool
    }

    // The datetime conventions a pattern belongs to.
    const (
    \tfmtModeNone      = 0
    \tfmtModeGregorian = 1
    \tfmtModeIsoweek   = 2
    )

    // The datetime suffix bits (PostgreSQL's DCH_S_*).
    const (
    #{bits}
    )

    // The datetime pattern actions.
    const (
    #{dch_ids}
    )

    // The number pattern actions.
    const (
    #{num_ids}
    )

    // dchKeywords is PostgreSQL's DCH_keywords, in its search order.
    var dchKeywords = []fmtKeyword{
    #{dch}
    }

    // numKeywords is PostgreSQL's NUM_keywords, in its search order.
    var numKeywords = []fmtKeyword{
    #{num}
    }

    // dchIndex / numIndex give the first keyword starting with each ASCII byte (-1: none).
    var dchIndex = [128]int16{#{index_of(data['dch']).join(', ')}}

    var numIndex = [128]int16{#{index_of(data['num']).join(', ')}}

    // dchSuffixes is PostgreSQL's DCH_suff.
    var dchSuffixes = []fmtSuffix{
    #{suff}
    }

    // The fixed English word lists.
    var (
    #{words}
    )
  GO
end

# --- Rust ----------------------------------------------------------------------

def rust_ident(prefix, id) = "#{prefix}_#{id}"

def rust_file(data)
  dch_ids = ids(data["dch"]).each_with_index.map { |id, i| "pub const #{rust_ident('DCH', id)}: u8 = #{i};" }.join("\n")
  num_ids = ids(data["num"]).each_with_index.map { |id, i| "pub const #{rust_ident('NUM', id)}: u8 = #{i};" }.join("\n")
  dch = data["dch"].map do |e|
    "    FmtKeyword { name: #{str_lit(e['name'])}, id: #{rust_ident('DCH', e['id'])}, is_digit: #{e['is_digit']}, date_mode: MODE_#{e['date_mode'].upcase} },"
  end.join("\n")
  num = data["num"].map do |e|
    "    FmtKeyword { name: #{str_lit(e['name'])}, id: #{rust_ident('NUM', e['id'])}, is_digit: false, date_mode: MODE_NONE },"
  end.join("\n")
  suff = data["suffixes"].map do |s|
    "    FmtSuffix { name: #{str_lit(s['name'])}, flag: SUFFIX_#{s['id']}, prefix: #{s['type'] == 'prefix'} },"
  end.join("\n")
  bits = SUFFIX_BITS.map { |k, v| "pub const SUFFIX_#{k}: u8 = 0x#{format('%02x', v)};" }.join("\n")
  words = WORD_LISTS.keys.map do |k|
    "pub const WORDS_#{k.upcase}: [&str; #{WORD_LISTS[k]}] = [#{data['words'][k].map { |w| str_lit(w) }.join(', ')}];"
  end.join("\n")
  <<~RS
    // @generated by scripts/gen_formatting.rb from spec/types/formatting.toml — DO NOT EDIT.
    //
    // PostgreSQL's data-type formatting tables (CLAUDE.md §5): DCH_keywords, DCH_suff, NUM_keywords,
    // and the fixed word lists, as DATA. The template parser and the processors that CONSUME them
    // are hand-written in formatting.rs. Regenerate with `mise run codegen`; `mise run verify`
    // fails if this file is stale. Reasoning: ../../../spec/design/formatting.md.
    #![allow(non_upper_case_globals)]

    /// One template pattern: `id` is its action; `is_digit` / `date_mode` are the datetime reader's
    /// facts (unused for number patterns).
    pub struct FmtKeyword {
        pub name: &'static str,
        pub id: u8,
        pub is_digit: bool,
        pub date_mode: u8,
    }

    /// One datetime modifier: `flag` is its suffix bit, `prefix` whether it precedes a pattern.
    pub struct FmtSuffix {
        pub name: &'static str,
        pub flag: u8,
        pub prefix: bool,
    }

    // The datetime conventions a pattern belongs to.
    pub const MODE_NONE: u8 = 0;
    pub const MODE_GREGORIAN: u8 = 1;
    pub const MODE_ISOWEEK: u8 = 2;

    // The datetime suffix bits (PostgreSQL's DCH_S_*).
    #{bits}

    // The datetime pattern actions.
    #{dch_ids}

    // The number pattern actions.
    #{num_ids}

    /// PostgreSQL's DCH_keywords, in its search order.
    #[rustfmt::skip]
    pub const DCH_KEYWORDS: &[FmtKeyword] = &[
    #{dch}
    ];

    /// PostgreSQL's NUM_keywords, in its search order.
    #[rustfmt::skip]
    pub const NUM_KEYWORDS: &[FmtKeyword] = &[
    #{num}
    ];

    /// The first keyword starting with each ASCII byte (-1: none).
    #[rustfmt::skip]
    pub const DCH_INDEX: [i16; 128] = [#{index_of(data['dch']).join(', ')}];

    #[rustfmt::skip]
    pub const NUM_INDEX: [i16; 128] = [#{index_of(data['num']).join(', ')}];

    /// PostgreSQL's DCH_suff.
    #[rustfmt::skip]
    pub const DCH_SUFFIXES: &[FmtSuffix] = &[
    #{suff}
    ];

    // The fixed English word lists.
    #[rustfmt::skip]
    #{words.gsub("\n", "\n#[rustfmt::skip]\n")}
  RS
end

# --- TypeScript ----------------------------------------------------------------

def ts_file(data)
  dch_ids = ids(data["dch"]).each_with_index.map { |id, i| "export const #{rust_ident('DCH', id)} = #{i};" }.join("\n")
  num_ids = ids(data["num"]).each_with_index.map { |id, i| "export const #{rust_ident('NUM', id)} = #{i};" }.join("\n")
  dch = data["dch"].map do |e|
    "  { name: #{str_lit(e['name'])}, id: #{rust_ident('DCH', e['id'])}, isDigit: #{e['is_digit']}, dateMode: MODE_#{e['date_mode'].upcase} },"
  end.join("\n")
  num = data["num"].map do |e|
    "  { name: #{str_lit(e['name'])}, id: #{rust_ident('NUM', e['id'])}, isDigit: false, dateMode: MODE_NONE },"
  end.join("\n")
  suff = data["suffixes"].map do |s|
    "  { name: #{str_lit(s['name'])}, flag: SUFFIX_#{s['id']}, prefix: #{s['type'] == 'prefix'} },"
  end.join("\n")
  bits = SUFFIX_BITS.map { |k, v| "export const SUFFIX_#{k} = 0x#{format('%02x', v)};" }.join("\n")
  words = WORD_LISTS.keys.map do |k|
    "export const WORDS_#{k.upcase}: readonly string[] = [#{data['words'][k].map { |w| str_lit(w) }.join(', ')}];"
  end.join("\n")
  <<~TS
    // @generated by scripts/gen_formatting.rb from spec/types/formatting.toml — DO NOT EDIT.
    //
    // PostgreSQL's data-type formatting tables (CLAUDE.md §5): DCH_keywords, DCH_suff, NUM_keywords,
    // and the fixed word lists, as DATA. The template parser and the processors that CONSUME them
    // are hand-written in formatting.ts. Regenerate with `mise run codegen`; `mise run verify`
    // fails if this file is stale. Reasoning: ../../../spec/design/formatting.md.

    // One template pattern: `id` is its action; `isDigit` / `dateMode` are the datetime reader's
    // facts (unused for number patterns).
    export interface FmtKeyword {
      name: string;
      id: number;
      isDigit: boolean;
      dateMode: number;
    }

    // One datetime modifier: `flag` is its suffix bit, `prefix` whether it precedes a pattern.
    export interface FmtSuffix {
      name: string;
      flag: number;
      prefix: boolean;
    }

    // The datetime conventions a pattern belongs to.
    export const MODE_NONE = 0;
    export const MODE_GREGORIAN = 1;
    export const MODE_ISOWEEK = 2;

    // The datetime suffix bits (PostgreSQL's DCH_S_*).
    #{bits}

    // The datetime pattern actions.
    #{dch_ids}

    // The number pattern actions.
    #{num_ids}

    // PostgreSQL's DCH_keywords, in its search order.
    export const DCH_KEYWORDS: readonly FmtKeyword[] = [
    #{dch}
    ];

    // PostgreSQL's NUM_keywords, in its search order.
    export const NUM_KEYWORDS: readonly FmtKeyword[] = [
    #{num}
    ];

    // The first keyword starting with each ASCII byte (-1: none).
    export const DCH_INDEX: readonly number[] = [#{index_of(data['dch']).join(', ')}];

    export const NUM_INDEX: readonly number[] = [#{index_of(data['num']).join(', ')}];

    // PostgreSQL's DCH_suff.
    export const DCH_SUFFIXES: readonly FmtSuffix[] = [
    #{suff}
    ];

    // The fixed English word lists.
    #{words}
  TS
end

# --- driver --------------------------------------------------------------------

def main
  check = ARGV.include?("--check")
  data = load_data
  stale = []
  TARGETS.each do |rel, builder|
    path = File.join(REPO, rel)
    content = send(builder, data)
    if check
      current = File.exist?(path) ? File.read(path) : nil
      stale << rel unless current == content
    else
      File.write(path, content)
      puts "wrote #{rel}"
    end
  end
  return unless check

  unless stale.empty?
    stale.each { |rel| warn "STALE: #{rel} — run 'mise run codegen'" }
    exit 1
  end
  puts "OK: #{TARGETS.length} generated files current " \
       "(#{data['dch'].length} datetime patterns, #{data['num'].length} number patterns)"
end

main
