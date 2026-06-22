# Vendored collation source — Unicode 17.0.0 / CLDR 48

The version-pinned canonical definitions the build-time pipeline compiles into the vendored `.coll`
artifacts (spec/design/collation.md §9). Committed and auditable; the cores never read these at
runtime (they read the compiled `spec/collation/fixtures/*.coll`).

| File | What | Source |
|---|---|---|
| `root.allkeys` | the CLDR-tailored DUCET root (the table ICU/PostgreSQL use), UCA/UCD **17.0.0** | CLDR 48 `common/uca/allkeys_CLDR.txt` (https://github.com/unicode-org/cldr/blob/release-48/common/uca/allkeys_CLDR.txt) |
| `es.ldml` | Spanish tailoring — `ñ` a distinct primary letter after `n` | CLDR 48 `common/collation/es.xml` `<collation type="standard">` |
| `casing.txt` | Unicode **case mappings** (simple + unconditional special), the JUCD property section (collation.md §16) | `UnicodeData.txt` (fields 12/13/14) + `SpecialCasing.txt`, derived by `scripts/extract_casing.rb` |

`root.allkeys` is the `allkeys.txt` line format (spec/collation/README.md §1.1) verbatim; `es.ldml` is
the LDML rule subset (§1.2); `casing.txt` is the compact case-mapping format (spec/collation/README.md
§5) that `compile_casing` reads into the bundle's property section. The casing common code points
(Latin, Greek, Cyrillic) are version-stable, so the **simple** mappings match the live `postgres:18`
oracle (glibc handles 1:1 mappings the same way); the **expanding** mappings (`ß`→`SS`) follow Unicode
SpecialCasing where glibc gives 1:1 — a documented divergence (collation.md §15/§16). Pinned to **Unicode 17.0.0** (the current version; what PostgreSQL 19's
ICU will use). The curated common code points (Latin, digits, the `es` ñ) are version-stable, so the
orderings still match the live `postgres:18` oracle's ICU 16.0 (icu_unicode_version 16.0) for the
cases the corpus checks — only newer/esoteric code points differ between 16.0 and 17.0.

Licensed under the Unicode License v3 (https://www.unicode.org/license.txt) / CLDR terms.

Coverage this slice: every **explicitly-listed** code point (all non-CJK scripts). CJK ideographs and
other `@implicitweights` ranges use UCA algorithmic derivation (implicit weights), which is the
deferred tier-3 follow-on — a string containing one raises `0A000` until then.
