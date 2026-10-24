// Cross-core regex compile-determinism + execution check (spec/design/regex.md §9). Reads the
// authored spec/regex/{program,match}_vectors.toml and asserts this core compiles each pattern to the
// exact instruction listing + class table + count (= regex_compile cost), and runs the VM to the
// exact match result, capture spans, and regex_step count. The Rust and Go cores run the equivalent
// check against the SAME files, pinning the three engines identical (CLAUDE.md §2/§8 — the byte-level
// contract the SQL conformance corpus cannot express, §10).

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { foldLowerSimple } from "../src/collation.ts";

// The vectors fold under the ASCII casing baseline: no Unicode property table is loaded.
const NO_PROPERTY = undefined;
import { Meter } from "../src/cost.ts";
import {
  compileRegex,
  compileRegexMode,
  compileSimilar,
  compileSimilarMatcher,
  foldRegexPattern,
  regexClassListing,
  regexIsMatch,
  regexListing,
  regexNinst,
  regexRun,
  regexSlots,
  similarNinst,
  similarSubstringSpan,
  similarToEscape,
} from "../src/regex.ts";
import { specPath } from "./tomlmini.ts";

type Block = Map<string, string>;

// compileCase compiles a vector's pattern; flags "m" selects the line-anchor mode (regex.md §8d).
function compileCase(pat: string, flags: string) {
  return flags.includes("m")
    ? compileRegexMode(pat, { nlStop: false, dotAll: false, nlAnchor: true })
    : compileRegex(pat);
}

// parseCaseBlocks splits a fixture into [[case]] blocks of key→raw-value, skipping comments/blanks.
function parseCaseBlocks(path: string): Block[] {
  const blocks: Block[] = [];
  let cur: Block | null = null;
  for (const raw of readFileSync(path, "utf8").split("\n")) {
    const line = raw.trim();
    if (line === "[[case]]") {
      cur = new Map();
      blocks.push(cur);
      continue;
    }
    if (line === "" || line.startsWith("#") || cur === null) continue;
    const eq = line.indexOf("=");
    if (eq < 0) continue;
    cur.set(line.slice(0, eq).trim(), line.slice(eq + 1).trim());
  }
  return blocks;
}

// tomlUnquote strips surrounding quotes and applies TOML basic-string \\ / \" / \n / \t unescaping.
function tomlUnquote(s: string | undefined): string {
  if (s === undefined) return "";
  s = s.trim();
  if (s.length < 2) return s;
  const inner = s.slice(1, -1);
  let out = "";
  for (let i = 0; i < inner.length; i++) {
    if (inner[i] === "\\" && i + 1 < inner.length) {
      i++;
      switch (inner[i]) {
        case "\\":
          out += "\\";
          break;
        case '"':
          out += '"';
          break;
        case "n":
          out += "\n";
          break;
        case "t":
          out += "\t";
          break;
        default:
          out += "\\" + inner[i];
      }
    } else {
      out += inner[i];
    }
  }
  return out;
}

// parseStrArray parses `["a", "b"]` (or `[]`) of quoted strings.
function parseStrArray(val: string | undefined): string[] {
  if (val === undefined) return [];
  const inner = val.trim().replace(/^\[/, "").replace(/\]$/, "");
  if (inner.trim() === "") return [];
  return inner.split(",").map((p) => tomlUnquote(p.trim()));
}

// parsePairs parses `[[0, 1], [2, 5]]` (or `[]`) into a flat [start, end, …] number list.
function parsePairs(val: string | undefined): number[] {
  if (val === undefined) return [];
  const out: number[] = [];
  for (const m of val.matchAll(/-?\d+/g)) out.push(Number(m[0]));
  return out;
}

test("regex program vectors match spec/regex/program_vectors.toml", () => {
  const blocks = parseCaseBlocks(specPath("regex/program_vectors.toml"));
  assert.ok(blocks.length >= 25, `expected the full vector set, got ${blocks.length}`);
  for (const c of blocks) {
    const pattern = tomlUnquote(c.get("pattern"));
    const pat = tomlUnquote(c.get("flags")).includes("i")
      ? foldRegexPattern(pattern, NO_PROPERTY)
      : pattern;
    const prog = compileCase(pat, tomlUnquote(c.get("flags")));
    assert.deepEqual(regexListing(prog), parseStrArray(c.get("prog")), `program for ${pattern}`);
    assert.deepEqual(
      regexClassListing(prog),
      parseStrArray(c.get("classes")),
      `classes for ${pattern}`,
    );
    assert.equal(regexNinst(prog), Number(c.get("count")), `count for ${pattern}`);
  }
});

test("regex match vectors match spec/regex/match_vectors.toml", () => {
  const blocks = parseCaseBlocks(specPath("regex/match_vectors.toml"));
  assert.ok(blocks.length >= 25, `expected the full vector set, got ${blocks.length}`);
  for (const c of blocks) {
    const pattern = tomlUnquote(c.get("pattern"));
    const input = tomlUnquote(c.get("input"));
    const insensitive = tomlUnquote(c.get("flags")).includes("i");
    const pat = insensitive ? foldRegexPattern(pattern, NO_PROPERTY) : pattern;
    const subj = insensitive ? foldLowerSimple(input, NO_PROPERTY) : input;
    const prog = compileCase(pat, tomlUnquote(c.get("flags")));
    const cps = Array.from(subj, (ch) => ch.codePointAt(0) as number);
    const m = new Meter();
    const caps = regexRun(prog, cps, m);
    const wantMatched = c.get("matched") === "true";
    assert.equal(caps !== null, wantMatched, `matched for ${pattern}/${input}`);
    const wantCaps = wantMatched ? parsePairs(c.get("caps")) : [];
    assert.deepEqual(
      caps === null ? [] : regexSlots(prog, caps),
      wantCaps,
      `caps for ${pattern}/${input}`,
    );
    assert.equal(m.accrued, BigInt(Number(c.get("steps"))), `steps for ${pattern}/${input}`);
  }
});

// The SIMILAR TO contract (regex.md §8f): the translation, the dot-all match, and the four-search
// substring, with their compile and step costs.
test("regex SIMILAR vectors match spec/regex/similar_vectors.toml", () => {
  const blocks = parseCaseBlocks(specPath("regex/similar_vectors.toml"));
  assert.ok(blocks.length >= 15, `expected the full vector set, got ${blocks.length}`);
  const code = (f: () => unknown): string => {
    try {
      f();
    } catch (e) {
      return (e as { code(): string }).code();
    }
    return "";
  };
  for (const c of blocks) {
    const pattern = tomlUnquote(c.get("pattern"));
    const input = tomlUnquote(c.get("input"));
    const escapeChar = c.has("escape") ? tomlUnquote(c.get("escape")) : undefined;
    const what = `${pattern} escape ${escapeChar} on ${input}`;
    if (c.has("error")) {
      assert.equal(
        code(() => similarToEscape(pattern, escapeChar)),
        tomlUnquote(c.get("error")),
        `error for ${what}`,
      );
      continue;
    }
    const translated = similarToEscape(pattern, escapeChar);
    assert.equal(translated, tomlUnquote(c.get("translation")), `translation for ${what}`);
    const cps = Array.from(input, (ch) => ch.codePointAt(0) as number);
    const prog = compileSimilar(translated);
    let m = new Meter();
    assert.equal(regexIsMatch(prog, cps, m), c.get("similar") === "true", `similar for ${what}`);
    assert.equal(regexNinst(prog), Number(c.get("similar_compile")), `similar_compile for ${what}`);
    assert.equal(m.accrued, BigInt(Number(c.get("similar_steps"))), `similar_steps for ${what}`);
    if (c.has("substring_error")) {
      assert.equal(
        code(() => compileSimilarMatcher(pattern, escapeChar ?? "\\")),
        tomlUnquote(c.get("substring_error")),
        `substring_error for ${what}`,
      );
      continue;
    }
    const sm = compileSimilarMatcher(pattern, escapeChar ?? "\\");
    m = new Meter();
    const span = similarSubstringSpan(sm, cps, m);
    const got = span === null ? null : String.fromCodePoint(...cps.slice(span[0], span[1]));
    const want = c.get("substring_null") === "true" ? null : tomlUnquote(c.get("substring"));
    assert.equal(got, want, `substring for ${what}`);
    assert.equal(
      similarNinst(sm),
      Number(c.get("substring_compile")),
      `substring_compile for ${what}`,
    );
    assert.equal(
      m.accrued,
      BigInt(Number(c.get("substring_steps"))),
      `substring_steps for ${what}`,
    );
  }
});
