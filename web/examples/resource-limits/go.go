package main

import (
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

	// Serve untrusted queries through a session bounded TWO ways:
	//   MaxCost         — a per-STATEMENT ceiling: one runaway query aborts 54P01.
	//   LifetimeMaxCost — a per-SESSION budget: the session's cumulative cost is capped, so a flood
	//                     of cheap queries can't burn unbounded CPU. It aborts 54P02.
	untrusted := db.Session(jed.SessionOptions{MaxCost: 10000, LifetimeMaxCost: 3})
	defer untrusted.Close()

	// Each statement accrues into the session's running total; read it with LifetimeCost().
	untrusted.Execute("SELECT 1", nil) // cost 1 — cumulative 1
	untrusted.Execute("SELECT 1", nil) // cost 1 — cumulative 2

	// The third drives the cumulative to the budget — the in-flight statement aborts 54P02, and the
	// partial cost still counts, so the session is now spent.
	if _, err := untrusted.Execute("SELECT 1", nil); err != nil {
		fmt.Println("denied:", err.(*jed.EngineError).Code()) // 54P02
	}
	fmt.Println("spent:", untrusted.LifetimeCost()) // 3 — the budget

	// Once spent, every further statement is rejected at admission — the session is done.
	if _, err := untrusted.Execute("SELECT 1", nil); err != nil {
		fmt.Println("admission:", err.(*jed.EngineError).Code()) // 54P02
	}
}
