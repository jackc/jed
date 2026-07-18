package main

import (
	"context"
	"fmt"
	"log"

	jed "github.com/jackc/jed/impl/go"
)

func main() {
	// A host registers its own SCALAR FUNCTIONS over the built-in types. Build a registry, add
	// functions, and hand it to Create/Open — the engine freezes it for the handle's lifetime and
	// shares it into every session. It is a handle setting, never written to the file (a reopening
	// host brings its own).
	registry := jed.NewExtensionRegistry()

	// discount(cents, pct) -> the price after a whole-percent discount. STRICT — a NULL argument
	// short-circuits to NULL before the kernel runs, so the closure never sees one — and reached by an
	// EXACT (i64, i64) signature (no implicit promotion; a built-in of the same signature would win).
	// WithCost(2) is charged once per call and gated against a session's MaxCost, so the function stays
	// inside the untrusted-query bound.
	err := registry.RegisterFunction(
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

	db, err := jed.CreateDatabase(jed.CreateOptions{Extensions: registry})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	ctx := context.Background()
	mustExec(db, "CREATE TABLE product (id i32 PRIMARY KEY, name text, price_cents i64)")
	mustExec(db, "INSERT INTO product VALUES (1, 'Mug', 1250), (2, 'Notebook', 400)")

	// Because discount is IMMUTABLE and carries a component identity, it can back a persisted index.
	// On reopen, if the registry supplies a different component/version, the index is skipped for
	// reads (a correct heap scan) and refused for writes — never a silently stale result.
	mustExec(db, "CREATE INDEX ON product (discount(price_cents, 10))")

	// Call it by name from SQL, exactly like a built-in.
	rows, err := db.Query(ctx, "SELECT name, discount(price_cents, 15) AS sale FROM product ORDER BY id")
	if err != nil {
		log.Fatal(err)
	}
	for rows.Next() {
		r := rows.Row()
		fmt.Printf("%s -> %s\n", r[0].Render(), r[1].Render()) // Mug -> 1063, Notebook -> 340
	}
}

func mustExec(db *jed.Database, sql string) {
	if _, err := db.Exec(context.Background(), sql); err != nil {
		log.Fatal(err)
	}
}
