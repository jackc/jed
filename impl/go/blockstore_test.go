package jed

import "testing"

func TestMemoryBlockStoreGrowsWithZeroFillAndCopiesReads(t *testing.T) {
	t.Parallel()
	store := newMemoryBlockStore([]byte{1, 2, 3})

	if err := store.setSize(6); err != nil {
		t.Fatal(err)
	}
	got, err := store.readAt(0, 6)
	if err != nil {
		t.Fatal(err)
	}
	assertBytes(t, got, []byte{1, 2, 3, 0, 0, 0})

	if err := store.writeAt(2, []byte{9, 8, 7}); err != nil {
		t.Fatal(err)
	}
	got, err = store.readAt(0, 6)
	if err != nil {
		t.Fatal(err)
	}
	assertBytes(t, got, []byte{1, 2, 9, 8, 7, 0})

	copyOfPrefix, err := store.readAt(0, 3)
	if err != nil {
		t.Fatal(err)
	}
	copyOfPrefix[0] = 99
	got, err = store.readAt(0, 3)
	if err != nil {
		t.Fatal(err)
	}
	assertBytes(t, got, []byte{1, 2, 9})

	if err := store.setSize(4); err != nil {
		t.Fatal(err)
	}
	size, err := store.size()
	if err != nil {
		t.Fatal(err)
	}
	if size != 4 {
		t.Fatalf("size = %d, want 4", size)
	}
	got, err = store.readAt(0, 4)
	if err != nil {
		t.Fatal(err)
	}
	assertBytes(t, got, []byte{1, 2, 9, 8})
}

func TestMemoryBlockStoreShortReadIsIoError(t *testing.T) {
	t.Parallel()
	store := newMemoryBlockStore([]byte{1, 2, 3})
	if _, err := store.readAt(2, 2); err == nil {
		t.Fatal("expected short read error")
	} else if ee, ok := err.(*EngineError); !ok || ee.Code() != "58030" {
		t.Fatalf("err = %v, want 58030", err)
	}
}

func assertBytes(t *testing.T, got, want []byte) {
	t.Helper()
	if string(got) != string(want) {
		t.Fatalf("bytes = %v, want %v", got, want)
	}
}
