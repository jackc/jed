// Engine auto-names (spec/design/grammar.md §3.1): the names jed derives for an unnamed
// constraint, index, or owned sequence — <table>_pkey, <table>_<cols>_key, …_fkey, …_check,
// …_excl, …_idx, <table>_<col>_seq. A port of PostgreSQL's makeObjectName / ChooseRelationName /
// ChooseConstraintName / ChooseIndexNameAddition / ChooseIndexColumnNames (indexcmds.c,
// pg_constraint.c), so a derived name always fits the 63-byte identifier limit
// (MAX_IDENTIFIER_LENGTH) and stays referenceable from SQL, byte for byte as PostgreSQL chooses
// it. Lengths are UTF-8 bytes (JS strings are UTF-16, so they are counted per code point); a cut
// never splits a character. Self-contained (no imports) so errors.ts can use it cycle-free.

// The lexer's MAX_IDENTIFIER_LENGTH (lexer.ts): PostgreSQL's NAMEDATALEN − 1.
const MAX_NAME_BYTES = 63;

// The UTF-8 width of the code point starting at UTF-16 index i, and its UTF-16 length.
function codePointWidth(s: string, i: number): [bytes: number, units: number] {
  const c = s.codePointAt(i)!;
  if (c < 0x80) return [1, 1];
  if (c < 0x800) return [2, 1];
  if (c < 0x10000) return [3, 1];
  return [4, 2];
}

// utf8Bytes is the UTF-8 byte length of s.
function utf8Bytes(s: string): number {
  let n = 0;
  for (let i = 0; i < s.length; ) {
    const [b, u] = codePointWidth(s, i);
    n += b;
    i += u;
  }
  return n;
}

// clipName is the longest prefix of s that is at most limit UTF-8 bytes and ends on a character
// boundary (PostgreSQL's pg_mbcliplen over UTF-8).
function clipName(s: string, limit: number): string {
  let n = 0;
  let i = 0;
  while (i < s.length) {
    const [b, u] = codePointWidth(s, i);
    if (n + b > limit) break;
    n += b;
    i += u;
  }
  return s.slice(0, i);
}

// makeObjectName is PostgreSQL's makeObjectName: name1[_name2]_label, shortening the longer of
// name1 / name2 one byte at a time until the whole fits in MAX_NAME_BYTES bytes (each part then
// clipped back to a character boundary). The label is never shortened. name2 null is PG's NULL.
export function makeObjectName(name1: string, name2: string | null, label: string): string {
  let overhead = utf8Bytes(label) + 1;
  let name1Len = utf8Bytes(name1);
  let name2Len = 0;
  if (name2 !== null) {
    name2Len = utf8Bytes(name2);
    overhead++;
  }
  const avail = MAX_NAME_BYTES - overhead;
  while (name1Len + name2Len > avail) {
    if (name1Len > name2Len) name1Len--;
    else name2Len--;
  }
  let out = clipName(name1, name1Len);
  if (name2 !== null) out += "_" + clipName(name2, name2Len);
  return out + "_" + label;
}

// chooseObjectName is PostgreSQL's ChooseRelationName / ChooseConstraintName loop: the first of
// makeObjectName(name1, name2, label), then with label1, label2, … — each re-truncated to fit —
// that taken reports free.
export function chooseObjectName(
  name1: string,
  name2: string | null,
  label: string,
  taken: (name: string) => boolean,
): string {
  let name = makeObjectName(name1, name2, label);
  for (let pass = 1; taken(name); pass++) {
    name = makeObjectName(name1, name2, label + pass.toString());
  }
  return name;
}

// nameAddition is PostgreSQL's ChooseIndexNameAddition (and the identical
// ChooseForeignKeyConstraintNameAddition): the column-name parts joined by "_", stopping once the
// result reaches 64 bytes (the makeObjectName cut does the rest).
export function nameAddition(names: readonly string[]): string {
  let out = "";
  for (const name of names) {
    if (out !== "") out += "_";
    out += clipName(name, MAX_NAME_BYTES);
    if (utf8Bytes(out) > MAX_NAME_BYTES) break;
  }
  return out;
}

// indexColumnNames is PostgreSQL's ChooseIndexColumnNames de-duplication: a part repeating an
// earlier one becomes part1, part2, … (the part clipped so the suffixed form fits MAX_NAME_BYTES)
// — (a, a) names a, a1.
export function indexColumnNames(parts: readonly string[]): string[] {
  const out: string[] = [];
  for (const orig of parts) {
    let cur = orig;
    for (let i = 1; out.includes(cur); i++) {
      const n = i.toString();
      cur = clipName(orig, MAX_NAME_BYTES - n.length) + n;
    }
    out.push(cur);
  }
  return out;
}

// pkeyName is the derived <table>_pkey constraint name PostgreSQL auto-assigns a primary key,
// truncated to fit 63 bytes. jed persists no such relation (constraints.md §5.4), so it takes no
// collision suffix.
export function pkeyName(table: string): string {
  return makeObjectName(table, null, "pkey");
}
