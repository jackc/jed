CREATE TABLE t (id int32 PRIMARY KEY, name text, score numeric(5,2));
INSERT INTO t VALUES (1, 'a,b', 1.50), (2, 'say "hi"', NULL), (3, NULL, 12.00);
SELECT * FROM t ORDER BY id;
