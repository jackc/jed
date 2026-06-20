// Lexer tokens. TokenKind is a string-literal union (not a TS enum — elidable subset).

export type TokenKind =
  | "word" // a bare word: keyword or identifier (compared case-insensitively)
  | "int" // an integer literal's unsigned magnitude (the sign is "minus")
  | "decimal" // a decimal literal (a numeric literal with a "."); see `decDigits`/`decScale`
  | "str" // a single-quoted string literal's decoded content (the text type)
  | "comma" // ,
  | "dot" // . — the separator of a qualified column reference (t.col); emitted only when a
  //         "." is NOT part of a numeric literal (spec/design/grammar.md §4/§15)
  | "lparen" // (
  | "rparen" // )
  | "lbracket" // [ — array subscript / ARRAY[…] / T[] type-suffix (spec/design/array.md)
  | "rbracket" // ]
  | "star" // *
  | "plus" // +
  | "minus" // -
  | "slash" // /
  | "percent" // %
  | "eq" // =
  | "ne" // <> (or its != alias) — the not-equal operator; the lexer folds both spellings to
  //         this one token (spec/design/grammar.md §4)
  | "lt" // <
  | "gt" // >
  | "le" // <=
  | "ge" // >=
  | "doubleColon" // :: — the PostgreSQL typecast operator (expr::type = CAST(expr AS type)); two
  //                colons scanned greedily (spec/design/grammar.md §37)
  | "colon" // : — the array-slice bound separator a[m:n] (spec/design/array.md §6)
  | "fatArrow" // => — the named-argument arrow (name => value, PostgreSQL named notation); two
  //              chars scanned greedily after "=", the legacy ":=" is not jed's surface (grammar.md §17)
  | "concat" // || — the array concatenation operator (a || b); two "|" scanned greedily, a lone
  //              "|" is a 42601 syntax error (jed has no bitwise-or) (spec/design/grammar.md §39)
  | "contains" // @> — the array containment operator (a @> b — does a contain b); "@" then ">"
  //              scanned greedily, a lone "@" is a 42601 syntax error (spec/design/grammar.md §40)
  | "containedBy" // <@ — the array contained-by operator (a <@ b — is a contained by b) (grammar.md §40)
  | "overlaps" // && — the array overlap operator (a && b — do a and b share an element); two "&"
  //              scanned greedily, a lone "&" is a 42601 syntax error (spec/design/grammar.md §40)
  | "strictlyLeft" // << — the range strictly-left operator (a << b). Two "<". See range-functions.md §3 (RF3).
  | "strictlyRight" // >> — the range strictly-right operator (a >> b). Two ">". See range-functions.md §3 (RF3).
  | "notExtendRight" // &< — the range not-extend-right operator (a &< b). "&" then "<". See range-functions.md §3.
  | "notExtendLeft" // &> — the range not-extend-left operator (a &> b). "&" then ">". See range-functions.md §3.
  | "adjacent" // -|- — the range adjacency operator (a -|- b). "-" "|" "-", scanned greedily and
  //              checked BEFORE the "--" line comment. See range-functions.md §3 (RF3).
  | "param" // a bind parameter $N — `paramIndex` holds the 1-based index (spec/design/api.md §5)
  | "eof"; // end of input

// Token is a lexed token. `word` holds the text for "word"; `int` holds the unsigned
// magnitude for "int" (a bigint, so 2^63 — i64's negated minimum — is representable;
// the parser folds the sign and traps a bare magnitude > i64's maximum).
export type Token = {
  kind: TokenKind;
  word?: string;
  int?: bigint;
  str?: string; // decoded content for "str"
  // For "decimal": the unscaled coefficient as a decimal-digit string (leading zeros allowed,
  // no sign) and the scale (fractional digit count). 1.50 → ("150", 2). The sign is "minus";
  // the cap check is at resolve (spec/design/grammar.md §14).
  decDigits?: string;
  decScale?: number;
  paramIndex?: number; // for "param": the 1-based bind-parameter index
};
