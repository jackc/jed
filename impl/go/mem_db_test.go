package jed

// memDB builds a fresh in-memory database, wrapping CreateDatabase with the infallible-in-memory
// convenience that is deliberately NOT part of the public core API (spec/design/api.md §2.1.1): an
// in-memory create cannot fail, so its always-nil error is unwrapped here. This is the test suite
// being the "caller who wants an infallible in-memory handle" the spec describes — a test helper,
// never public surface.
func memDB() *Database {
	db, err := CreateDatabase(CreateOptions{})
	if err != nil {
		panic("in-memory CreateDatabase is infallible: " + err.Error())
	}
	return db
}
