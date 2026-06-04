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
  | "star" // *
  | "plus" // +
  | "minus" // -
  | "slash" // /
  | "percent" // %
  | "eq" // =
  | "lt" // <
  | "gt" // >
  | "le" // <=
  | "ge" // >=
  | "eof"; // end of input

// Token is a lexed token. `word` holds the text for "word"; `int` holds the unsigned
// magnitude for "int" (a bigint, so 2^63 — int64's negated minimum — is representable;
// the parser folds the sign and traps a bare magnitude > int64's maximum).
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
};
