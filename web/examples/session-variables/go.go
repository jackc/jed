package main

import (
	"fmt"
	"log"

	jed "github.com/jackc/jed/impl/go"
)

func main() {
	db, err := jed.CreateDatabase("app.jed", jed.DatabaseOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	// Session variables are PostgreSQL's GUC model, scoped to the session: a string→string map the
	// host sets and SQL reads with current_setting(). A custom variable must be NAMESPACED — a dotted
	// name like myapp.tenant; a non-dotted name is 42704.
	if err := db.SetVar("myapp.tenant", "acme"); err != nil {
		log.Fatal(err)
	}

	// Read it back through the host API — the name is case-insensitive; ok is false if it is unset.
	if v, ok := db.Var("myapp.tenant"); ok {
		fmt.Println("tenant:", v) // acme
	}

	// ... or in SQL with current_setting(): SELECT current_setting('myapp.tenant') -> "acme".
	if _, err := db.Query("SELECT current_setting('myapp.tenant')", nil); err != nil {
		log.Fatal(err)
	}

	// An unset name is 42704, unless the two-arg form passes missing_ok = true, which returns NULL:
	//   SELECT current_setting('myapp.unset')        -- 42704
	//   SELECT current_setting('myapp.unset', true)  -- NULL

	// Variables are SESSION state, not data — they do NOT roll back with a transaction. ResetVar
	// clears one by name.
	if err := db.ResetVar("myapp.tenant"); err != nil {
		log.Fatal(err)
	}
}
