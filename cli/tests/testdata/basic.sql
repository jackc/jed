-- The basic surface: DDL, multi-row INSERT, an aligned SELECT, transaction tags,
-- comments (engine-side, grammar.md §33), and multi-line statements.
CREATE TABLE users (
  id i64 PRIMARY KEY,
  name text NOT NULL,
  score numeric(5,2) DEFAULT 0
);
INSERT INTO users VALUES (1, 'alice', 9.50), (2, 'bob', NULL); -- seed rows
SELECT * FROM users ORDER BY id;
BEGIN;
UPDATE users SET score = 10.00 WHERE id = 1;
SELECT id, score FROM users /* inline */ WHERE id = 1;
ROLLBACK;
SELECT count(*) AS n FROM users
