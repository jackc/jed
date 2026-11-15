//go:build unix

package jed

import (
	"context"
	"runtime"
	"testing"
	"time"
)

func TestContextCancelsPageClientWriterWait(t *testing.T) {
	owner, client := remoteFixture(t)
	blocker := mustSession(owner, SessionOptions{})
	defer blocker.Close()
	if err := blocker.Begin(t.Context(), true); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(t.Context())
	defer cancel()
	finished := make(chan error, 1)
	go func() {
		_, err := client.Exec(ctx, "INSERT INTO t VALUES (3000, 'canceled')")
		finished <- err
	}()
	// Synchronize at the owner's pending request, while the blocker still owns
	// the gate. Cancellation must complete without closing this client's connection.
	deadline := time.Now().Add(5 * time.Second)
	waiting := false
	for !waiting && time.Now().Before(deadline) {
		for _, sc := range owner.core.pageOwner().connections() {
			sc.mu.Lock()
			waiting = waiting || sc.waiting
			sc.mu.Unlock()
		}
		runtime.Gosched()
	}
	if !waiting {
		t.Fatal("client never waited for the owner's gate")
	}
	cancel()
	select {
	case err := <-finished:
		requireContextError(t, err, context.Canceled)
	case <-time.After(5 * time.Second):
		t.Fatal("client wait did not cancel")
	}
	if client.core.role.Load().client.brokenErr() != nil {
		t.Fatal("cancellation broke the shared client connection")
	}
	var count int64
	if err := client.QueryRow(t.Context(), "SELECT count(*) FROM t").Scan(&count); err != nil || count != 2000 {
		t.Fatalf("client read after cancellation: %d, %v", count, err)
	}
	if err := blocker.Rollback(); err != nil {
		t.Fatal(err)
	}
	if _, err := client.Exec(t.Context(), "INSERT INTO t VALUES (3000, 'retry')"); err != nil {
		t.Fatal(err)
	}
}

func TestContextCancelsOwnerWriterWait(t *testing.T) {
	owner, client := remoteFixture(t)
	blocker := mustSession(client, SessionOptions{})
	defer blocker.Close()
	if err := blocker.Begin(t.Context(), true); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(t.Context())
	defer cancel()
	finished := make(chan error, 1)
	go func() { _, err := owner.Exec(ctx, "INSERT INTO t VALUES (3000, 'canceled')"); finished <- err }()
	deadline := time.Now().Add(5 * time.Second)
	held := false
	for !held && time.Now().Before(deadline) {
		owner.core.writeMu.mu.Lock()
		held = owner.core.writeMu.held
		owner.core.writeMu.mu.Unlock()
		runtime.Gosched()
	}
	if !held {
		t.Fatal("owner never began acquiring the service gate")
	}
	cancel()
	select {
	case err := <-finished:
		requireContextError(t, err, context.Canceled)
	case <-time.After(5 * time.Second):
		t.Fatal("owner wait did not cancel")
	}
	if err := blocker.Rollback(); err != nil {
		t.Fatal(err)
	}
	if _, err := owner.Exec(t.Context(), "INSERT INTO t VALUES (3000, 'retry')"); err != nil {
		t.Fatal(err)
	}
}
