// Lexer tokens. TokenKind is a string-literal union (not a TS enum — elidable subset).

export type TokenKind =
  | "word" // a bare word: keyword or identifier (compared case-insensitively)
  | "int" // an integer literal already parsed to bigint
  | "comma" // ,
  | "lparen" // (
  | "rparen" // )
  | "star" // *
  | "eq" // =
  | "lt" // <
  | "gt" // >
  | "le" // <=
  | "ge" // >=
  | "eof"; // end of input

// Token is a lexed token. `word` holds the text for "word"; `int` holds the value for
// "int". A bigint so int64 literals are exact.
export type Token = {
  kind: TokenKind;
  word?: string;
  int?: bigint;
};
