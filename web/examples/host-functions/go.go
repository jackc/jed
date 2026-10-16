package main

import (
	"context"
	"fmt"
	"log"

	jed "github.com/jackc/jed/impl/go"
)

func main() {
	// A host registers its own SCALAR FUNCTIONS over the built-in types on an EnvironmentBuilder,
	// builds the (immutable) Environment, and hands it to Create/Open — every session of the handle
	// shares it. It is never written to the file (a reopening host brings its own).
	builder := jed.NewEnvironmentBuilder()

	// discount(cents, pct) -> the price after a whole-percent discount. STRICT — a NULL argument
	// short-circuits to NULL before the kernel runs, so the closure never sees one — and reached by an
	// EXACT (i64, i64) signature (no implicit promotion; a built-in of the same signature would win).
	// WithCost(2) is charged once per call and gated against a session's MaxCost, so the function stays
	// inside the untrusted-query bound.
	err := builder.RegisterFunction(
		jed.NewHostFunction("discount", []string{"i64", "i64"}, "i64",
			func(args []jed.Value) (jed.Value, error) {
				cents, pct := args[0].Int, args[1].Int
				return jed.IntValue(cents - cents*pct/100), nil
			}).
			WithVolatility(jed.VolatilityImmutable).      // same inputs ⇒ same output
			WithCrossCore(true).                          // results are byte-identical on every core
			WithCost(2).                                  //
			WithComponentID("com.example/discount").      // a stable identity for index-backing
			WithSemanticVersion(1))                       // bump when a formula change invalidates keys
	if err != nil {
		log.Fatal(err)
	}

	// with_tax(cents) -> cents plus 8% tax, as a BATCH kernel: it receives a whole column of rows per
	// call (args[0][i] is row i's argument) and appends one result per row to out. jed calls it once
	// per chunk where it already holds the rows, and one row at a time elsewhere — same rows, cost, and
	// errors either way.
	err = builder.RegisterFunction(
		jed.NewHostBatchFunction("with_tax", []string{"i64"}, "i64",
			func(args [][]jed.Value, out []jed.Value) ([]jed.Value, error) {
				for _, v := range args[0] {
					out = append(out, jed.IntValue(v.Int+v.Int*8/100))
				}
				return out, nil
			}).
			WithVolatility(jed.VolatilityImmutable))
	if err != nil {
		log.Fatal(err)
	}

	db, err := jed.CreateDatabase(jed.CreateOptions{Environment: builder.Build()})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	ctx := context.Background()
	mustExec(db, "CREATE TABLE product (id i32 PRIMARY KEY, name text, price_cents i64)")
	mustExec(db, "INSERT INTO product VALUES (1, 'Mug', 1250), (2, 'Notebook', 400)")

	// Because discount is IMMUTABLE and carries a component identity, it can back a persisted index.
	// On reopen, if the environment supplies a different component/version, the index is skipped for
	// reads (a correct heap scan) and refused for writes — never a silently stale result.
	mustExec(db, "CREATE INDEX ON product (discount(price_cents, 10))")

	// Call it by name from SQL, exactly like a built-in.
	rows, err := db.Query(ctx,
		"SELECT name, discount(price_cents, 15) AS sale, with_tax(price_cents) AS total FROM product ORDER BY id")
	if err != nil {
		log.Fatal(err)
	}
	for rows.Next() {
		r := rows.Row()
		// Mug -> 1063 / 1350, Notebook -> 340 / 432
		fmt.Printf("%s -> %s / %s\n", r[0].Render(), r[1].Render(), r[2].Render())
	}
}

func mustExec(db *jed.Database, sql string) {
	if _, err := db.Exec(context.Background(), sql); err != nil {
		log.Fatal(err)
	}
}
