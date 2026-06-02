// Lexer tokens. TokenKind is a string-literal union (not a TS enum — elidable subset).

export type TokenKind =
  | "word" // a bare word: keyword or identifier (compared case-insensitively)
  | "int" // an integer literal's unsigned magnitude (the sign is "minus")
  | "comma" // ,
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
};
