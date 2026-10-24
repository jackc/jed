//go:build unix

package jed

// The stream transport's client (spec/pageservice/README.md §5.3) on what the real-process corpus
// cannot observe: read-ahead's round trips and the wording of a failed open. The transport here is
// this test binary relaying its stdio to a Go owner's endpoint (the relay half of `jed pages`), so the
// test needs no Rust serving end.

import (
	"io"
	"net"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// relayEnv names the endpoint a transport run of this binary relays its stdio to.
const relayEnv = "JED_TEST_RELAY_ENDPOINT"

func TestMain(m *testing.M) {
	if endpoint := os.Getenv(relayEnv); endpoint != "" {
		relayStdio(endpoint)
		os.Exit(0)
	}
	os.Exit(m.Run())
}

// relayStdio relays stdin to endpoint and endpoint to stdout until either side ends.
func relayStdio(endpoint string) {
	conn, err := net.Dial("unix", endpoint)
	if err != nil {
		os.Stderr.WriteString("relay: " + err.Error() + "\n")
		os.Exit(1)
	}
	done := make(chan struct{}, 2)
	go func() {
		_, _ = io.Copy(conn, os.Stdin)
		done <- struct{}{}
	}()
	go func() {
		_, _ = io.Copy(os.Stdout, conn)
		done <- struct{}{}
	}()
	<-done
}

// remoteFixture creates a served database of 2000 rows of 200 bytes at a fresh path and opens a stream
// client to it through the relay. The owner and the client are closed when the test ends.
func remoteFixture(t *testing.T) (owner, remote *Database) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "remote.jed")
	owner, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = owner.Close() })
	for _, sql := range []string{
		"CREATE TABLE t (id i64 PRIMARY KEY, v text)",
		"INSERT INTO t SELECT g, repeat('x', 200) FROM generate_series(1, 2000) g",
	} {
		if _, err := owner.Exec(t.Context(), sql); err != nil {
			t.Fatal(err)
		}
	}
	real, err := resolveServicePath(path)
	if err != nil {
		t.Fatal(err)
	}
	record, ok := readOwnerRecord(real)
	if !ok || !record.serving {
		t.Fatal("the owner wrote no serving owner record")
	}
	t.Setenv(relayEnv, record.endpoint)
	timeout := uint64(5000)
	remote, err = OpenRemote([]string{os.Args[0]}, "remote:"+path, OpenOptions{FileLockTimeoutMs: &timeout})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = remote.Close() })
	if role := PageServiceRoleForTesting(remote); role != "client" {
		t.Fatalf("the stream handle is %s, not a client", role)
	}
	return owner, remote
}

// drain runs sql and returns its first row's values and its cost.
func drain(t *testing.T, db *Database, sql string) ([]Value, int64) {
	t.Helper()
	rows, err := db.Query(t.Context(), sql)
	if err != nil {
		t.Fatal(err)
	}
	var first []Value
	for rows.Next() {
		if first == nil {
			first = rows.Row()
		}
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	return first, rows.Cost()
}

// Scans over the stream transport read ahead: a full scan fetches its leaves in batches, a scan
// stopped by LIMIT fetches no leaf beyond the first, and the cost is the owner's.
func TestRemoteScansReadAheadWithoutChangingCost(t *testing.T) {
	owner, remote := remoteFixture(t)
	full := "SELECT sum(length(v))::i64 FROM t"
	first := "SELECT id FROM t ORDER BY id LIMIT 1"
	_, fullCost := drain(t, owner, full)
	_, firstCost := drain(t, owner, first)

	before := psReadPagesSent.Load()
	if _, cost := drain(t, remote, first); cost != firstCost {
		t.Errorf("LIMIT 1 costs %d remotely, %d locally", cost, firstCost)
	}
	if sent := psReadPagesSent.Load() - before; sent != 1 {
		t.Errorf("LIMIT 1 sent %d read_pages requests, want 1 (one leaf)", sent)
	}

	before = psReadPagesSent.Load()
	row, cost := drain(t, remote, full)
	if cost != fullCost {
		t.Errorf("the full scan costs %d remotely, %d locally", cost, fullCost)
	}
	if len(row) != 1 || row[0] != IntValue(2000*200) {
		t.Errorf("the full scan returned %v", row)
	}
	leaves := uint64(2000 * 200 / 4096)
	if sent := psReadPagesSent.Load() - before; sent*4 >= leaves {
		t.Errorf("%d read_pages requests for about %d leaves", sent, leaves)
	}
}

// A write through the stream client commits at the owner.
func TestRemoteWriteCommitsAtTheOwner(t *testing.T) {
	owner, remote := remoteFixture(t)
	if _, err := remote.Exec(t.Context(), "INSERT INTO t VALUES (5000, 'remote')"); err != nil {
		t.Fatal(err)
	}
	if row, _ := drain(t, owner, "SELECT count(*) FROM t"); len(row) != 1 || row[0] != IntValue(2001) {
		t.Errorf("the owner counts %v rows", row)
	}
}

// A transport that cannot connect fails the open 08006 with its standard error.
func TestFailingTransportReportsItsStderr(t *testing.T) {
	timeout := uint64(300)
	_, err := OpenRemote([]string{"sh", "-c", "echo 'ssh: connect to host nowhere: Connection refused' >&2; exit 255"}, "remote:nowhere", OpenOptions{FileLockTimeoutMs: &timeout})
	engine, ok := err.(*EngineError)
	if !ok || engine.Code() != "08006" || !strings.Contains(engine.Error(), "Connection refused") {
		t.Fatalf("a failing transport opened with %v", err)
	}
}

// A transport command that cannot run fails the open 08006 naming it.
func TestMissingTransportFailsTheOpen(t *testing.T) {
	timeout := uint64(300)
	_, err := OpenRemote([]string{"/nonexistent/jed-transport"}, "remote:missing", OpenOptions{FileLockTimeoutMs: &timeout})
	engine, ok := err.(*EngineError)
	if !ok || engine.Code() != "08006" || !strings.Contains(engine.Error(), "could not run /nonexistent/jed-transport") {
		t.Fatalf("a missing transport opened with %v", err)
	}
}
