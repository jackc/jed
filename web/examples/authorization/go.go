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
	mustExec(db, "CREATE TABLE report (id i32 PRIMARY KEY, body text)")
	mustExec(db, "INSERT INTO report VALUES (1, 'hello')")

	// Serve untrusted queries through a SESSION granted ONLY read access: DefaultPrivileges =
	// {SELECT} (a read-only envelope) with DDL disabled. A session is a handle minted from the
	// database that shares its committed state. The engine enforces the envelope at name resolution —
	// any write or schema change resolves to 42501, with no in-database role catalog.
	readOnly := jed.PrivSetEmpty.With(jed.PrivSelect)
	noDDL := false
	untrusted := db.Session(jed.SessionOptions{DefaultPrivileges: &readOnly, AllowDDL: &noDDL})
	defer untrusted.Close() // release the session (and its reader pin)
	if _, err := untrusted.Execute("SELECT body FROM report", nil); err != nil {
		log.Fatal(err)
	}
	if _, err := untrusted.Execute("DELETE FROM report", nil); err != nil {
		fmt.Println("denied:", err) // 42501 permission denied for table report
	}

	// Grant/Revoke adjust one object at a time, and revoke always wins. Revoke EXECUTE on a volatile
	// function to pin a session's determinism — calls to it then fail 42501.
	db.Revoke(jed.PrivSetEmpty.With(jed.PrivExecute), "uuidv4")
}

func mustExec(db *jed.Database, sql string) {
	if _, err := db.Execute(sql, nil); err != nil {
		log.Fatal(err)
	}
}
