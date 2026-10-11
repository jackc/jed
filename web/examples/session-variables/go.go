package main

import (
	"context"
	"fmt"
	"log"

	jed "github.com/jackc/jed/impl/go"
)

func main() {
	db, err := jed.CreateDatabase(jed.CreateOptions{Path: "app.jed"})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	ctx := context.Background()

	// Session variables are PostgreSQL's GUC model — they live on a SESSION, so mint one from the
	// database rather than using the bare handle. A custom variable must be NAMESPACED — a dotted name
	// like myapp.tenant; a non-dotted name is 42704.
	s := db.Session(jed.SessionOptions{})
	defer s.Close()
	if err := s.SetVar("myapp.tenant", "acme"); err != nil {
		log.Fatal(err)
	}

	// Read it back through the host API — the name is case-insensitive; ok is false if it is unset.
	if v, ok := s.Var("myapp.tenant"); ok {
		fmt.Println("tenant:", v) // acme
	}

	// ... or in SQL with current_setting(): SELECT current_setting('myapp.tenant') -> "acme".
	if _, err := s.Exec(ctx, "SELECT current_setting('myapp.tenant')"); err != nil {
		log.Fatal(err)
	}

	// An unset name is 42704, unless the two-arg form passes missing_ok = true, which returns NULL:
	//   SELECT current_setting('myapp.unset')        -- 42704
	//   SELECT current_setting('myapp.unset', true)  -- NULL

	// Variables are SESSION state, not data — they do NOT roll back with a transaction. ResetVar
	// clears one by name.
	if err := s.ResetVar("myapp.tenant"); err != nil {
		log.Fatal(err)
	}

	// The built-in session time zone (UTC by default) decides how a timestamptz renders as text. A
	// query returns a zone-free value; render it through the session to show it in the session zone.
	if err := s.SetTimeZone("+05:00"); err != nil { // a fixed offset uses the POSIX sign: UTC-5
		log.Fatal(err)
	}
	rows, err := s.Query(ctx, "SELECT timestamptz '2024-01-15 12:00:00+00'")
	if err != nil {
		log.Fatal(err)
	}
	defer rows.Close()
	if rows.Next() {
		at := rows.Value(0)
		text, err := s.Render(at)
		if err != nil {
			log.Fatal(err)
		}
		fmt.Println(text)        // 2024-01-15 07:00:00-05
		fmt.Println(at.Render()) // 2024-01-15 12:00:00+00 — a Value's own render is always UTC
	}
}
