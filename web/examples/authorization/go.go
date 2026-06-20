package main

import (
	"fmt"
	"log"

	"jed"
)

func main() {
	db, err := jed.Create("app.jed", jed.DatabaseOptions{})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	jed.Execute(db, "CREATE TABLE report (id i32 PRIMARY KEY, body text)")
	jed.Execute(db, "INSERT INTO report VALUES (1, 'hello')")

	// Serve untrusted queries through a session granted ONLY read access: DefaultPrivileges =
	// {SELECT} (a read-only envelope) with DDL disabled. The engine enforces this at name
	// resolution — any write or schema change resolves to 42501, with no in-database role catalog.
	readOnly := jed.PrivSetEmpty.With(jed.PrivSelect)
	noDDL := false
	untrusted := db.NewSession(jed.SessionOptions{DefaultPrivileges: &readOnly, AllowDDL: &noDDL})
	if _, err := untrusted.ExecuteSQL(db, "SELECT body FROM report", nil); err != nil {
		log.Fatal(err)
	}
	if _, err := untrusted.ExecuteSQL(db, "DELETE FROM report", nil); err != nil {
		fmt.Println("denied:", err) // 42501 permission denied for table report
	}

	// Grant/Revoke adjust one object at a time, and revoke always wins. Revoke EXECUTE on a volatile
	// function to pin a session's determinism — calls to it then fail 42501.
	db.Revoke(jed.PrivSetEmpty.With(jed.PrivExecute), "uuidv4")
}
