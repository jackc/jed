package jed

import (
	"bytes"
	"encoding/binary"
	"sort"
)

// GIN posting runs (spec/design/gin.md §4.5, format_version 43). Mirrors impl/rust/src/gin_runs.rs.
//
// A GIN entry tree stores each term's posting list as runs: one record key holds the term, the run's
// last (largest) storage key, then the run's earlier storage keys front-coded, then the coded length.
// The record keys of one term sort by their last storage key, so the run that holds — or would hold —
// a storage key k is the first record at or after term ‖ k, and every term's runs are the prefix range
// [term, successor(term)). A from-scratch build packs each posting list greedily, and maintenance
// edits one run at a time by the rules below, so the same statements produce the same records in
// every core.

// ginRunMaxBytes is the longest run key maintenance builds by adding a storage key to a run
// (gin.md §4.5). A run of one storage key may be longer.
const ginRunMaxBytes = 64

func ginRunCorrupt() error { return newError(DataCorrupted, "a GIN posting run is malformed") }

func ginPutVarint(out []byte, v int) []byte {
	for {
		b := byte(v & 0x7F)
		v >>= 7
		if v == 0 {
			return append(out, b)
		}
		out = append(out, b|0x80)
	}
}

func ginReadVarint(b []byte, at *int) (int, error) {
	v := 0
	for shift := 0; shift < 21; shift += 7 {
		if *at >= len(b) {
			return 0, ginRunCorrupt()
		}
		c := b[*at]
		*at++
		v |= int(c&0x7F) << shift
		if c&0x80 == 0 {
			return v, nil
		}
	}
	return 0, ginRunCorrupt()
}

func ginSharedPrefix(a, b []byte) int {
	n := 0
	for n < len(a) && n < len(b) && a[n] == b[n] {
		n++
	}
	return n
}

// ginEncodeRun is the run key of term over the ascending, distinct storage keys (non-empty):
// term ‖ last ‖ body ‖ u16 BE len(body), the body front-coding every key but the last, in ascending
// order, each as varint(shared) ‖ varint(suffix length) ‖ suffix against the previous key (the first
// against the last).
func ginEncodeRun(term []byte, keys [][]byte) []byte {
	last := keys[len(keys)-1]
	var body []byte
	prev := last
	for _, k := range keys[:len(keys)-1] {
		shared := ginSharedPrefix(prev, k)
		body = ginPutVarint(body, shared)
		body = ginPutVarint(body, len(k)-shared)
		body = append(body, k[shared:]...)
		prev = k
	}
	out := make([]byte, 0, len(term)+len(last)+len(body)+2)
	out = append(out, term...)
	out = append(out, last...)
	out = append(out, body...)
	return binary.BigEndian.AppendUint16(out, uint16(len(body)))
}

// ginDecodeRun is the ascending storage keys of a run key whose term is termLen bytes long. A
// malformed run is XX001.
func ginDecodeRun(termLen int, run []byte) ([][]byte, error) {
	n := len(run)
	if n < termLen+3 {
		return nil, ginRunCorrupt()
	}
	bodyLen := int(binary.BigEndian.Uint16(run[n-2:]))
	bodyEnd := n - 2
	lastEnd := bodyEnd - bodyLen
	if lastEnd <= termLen {
		return nil, ginRunCorrupt()
	}
	last := run[termLen:lastEnd]
	body := run[lastEnd:bodyEnd]
	var keys [][]byte
	at := 0
	for at < len(body) {
		shared, err := ginReadVarint(body, &at)
		if err != nil {
			return nil, err
		}
		suffix, err := ginReadVarint(body, &at)
		if err != nil {
			return nil, err
		}
		prev := last
		if len(keys) > 0 {
			prev = keys[len(keys)-1]
		}
		if shared > len(prev) || at+suffix > len(body) || shared+suffix == 0 {
			return nil, ginRunCorrupt()
		}
		key := append(append([]byte{}, prev[:shared]...), body[at:at+suffix]...)
		at += suffix
		if len(keys) > 0 && bytes.Compare(keys[len(keys)-1], key) >= 0 {
			return nil, ginRunCorrupt()
		}
		keys = append(keys, key)
	}
	if len(keys) > 0 && bytes.Compare(keys[len(keys)-1], last) >= 0 {
		return nil, ginRunCorrupt()
	}
	return append(keys, append([]byte{}, last...)), nil
}

// ginPackRuns is the runs of term over ascending, distinct keys, packed greedily: a run takes each
// next key while its key stays within ginRunMaxBytes (a from-scratch build, gin.md §4.5).
func ginPackRuns(term []byte, keys [][]byte) [][]byte {
	var runs [][]byte
	for start := 0; start < len(keys); {
		end := start + 1
		for end < len(keys) && len(ginEncodeRun(term, keys[start:end+1])) <= ginRunMaxBytes {
			end++
		}
		runs = append(runs, ginEncodeRun(term, keys[start:end]))
		start = end
	}
	return runs
}

// ginSplitRuns is the runs of a run's key list after an edit: one run when it fits, else the first
// ceil(n/2) keys and the rest, each split again the same way until it fits or holds one key.
func ginSplitRuns(term []byte, keys [][]byte, out [][]byte) [][]byte {
	run := ginEncodeRun(term, keys)
	if len(run) <= ginRunMaxBytes || len(keys) == 1 {
		return append(out, run)
	}
	mid := (len(keys) + 1) / 2
	out = ginSplitRuns(term, keys[:mid], out)
	return ginSplitRuns(term, keys[mid:], out)
}

func ginTermBound(term []byte) keyBound {
	return keyBound{lo: term, loInc: true, hi: prefixSuccessor(term), hiInc: false}
}

// ginRunAtOrAfter is the first run key of term at or after term ‖ key — the run holding key if any
// holds it.
func ginRunAtOrAfter(store *tableStore, term, key []byte) ([]byte, error) {
	b := ginTermBound(term)
	b.lo = append(append([]byte{}, term...), key...)
	var found []byte
	err := store.ScanRange(b, func(k []byte, _ storedRow) (bool, error) {
		found = append([]byte{}, k...)
		return false, nil
	})
	return found, err
}

// ginRunBefore is the last run key of term before term ‖ key.
func ginRunBefore(store *tableStore, term, key []byte) ([]byte, error) {
	b := ginTermBound(term)
	b.hi = append(append([]byte{}, term...), key...)
	var found []byte
	err := store.ScanRangeRev(b, func(k []byte, _ storedRow) (bool, error) {
		found = append([]byte{}, k...)
		return false, nil
	})
	return found, err
}

func ginReplaceRun(store *tableStore, old []byte, runs [][]byte) error {
	if _, err := store.Remove(old); err != nil {
		return err
	}
	for _, r := range runs {
		if _, err := store.Insert(r, nil); err != nil {
			return err
		}
	}
	return nil
}

func ginSearchKeys(keys [][]byte, key []byte) (int, bool) {
	i := sort.Search(len(keys), func(i int) bool { return bytes.Compare(keys[i], key) >= 0 })
	return i, i < len(keys) && bytes.Equal(keys[i], key)
}

// ginInsert adds the posting (term, key) (gin.md §4.5). The run at or after term ‖ key takes the key
// (it holds a larger last key), split in halves when it outgrows ginRunMaxBytes. A key above every run
// joins the term's last run when that run stays within the limit, else starts a new run. false when
// the posting is already present.
func ginInsert(store *tableStore, term, key []byte) (bool, error) {
	run, err := ginRunAtOrAfter(store, term, key)
	if err != nil {
		return false, err
	}
	if run != nil {
		keys, err := ginDecodeRun(len(term), run)
		if err != nil {
			return false, err
		}
		at, found := ginSearchKeys(keys, key)
		if found {
			return false, nil
		}
		keys = append(keys[:at], append([][]byte{append([]byte{}, key...)}, keys[at:]...)...)
		return true, ginReplaceRun(store, run, ginSplitRuns(term, keys, nil))
	}
	prev, err := ginRunBefore(store, term, key)
	if err != nil {
		return false, err
	}
	if prev != nil {
		keys, err := ginDecodeRun(len(term), prev)
		if err != nil {
			return false, err
		}
		grown := ginEncodeRun(term, append(keys, append([]byte{}, key...)))
		if len(grown) <= ginRunMaxBytes {
			return true, ginReplaceRun(store, prev, [][]byte{grown})
		}
	}
	return store.Insert(ginEncodeRun(term, [][]byte{key}), nil)
}

// ginRemove removes the posting (term, key) (gin.md §4.5): its run loses the key, and an emptied run
// is removed. false when the posting is absent.
func ginRemove(store *tableStore, term, key []byte) (bool, error) {
	run, err := ginRunAtOrAfter(store, term, key)
	if err != nil || run == nil {
		return false, err
	}
	keys, err := ginDecodeRun(len(term), run)
	if err != nil {
		return false, err
	}
	at, found := ginSearchKeys(keys, key)
	if !found {
		return false, nil
	}
	keys = append(keys[:at], keys[at+1:]...)
	var runs [][]byte
	if len(keys) > 0 {
		runs = ginSplitRuns(term, keys, nil)
	}
	return true, ginReplaceRun(store, run, runs)
}

// indexEntry is one logical index entry with the length of its storage-key suffix.
type indexEntry struct {
	key     []byte
	skeyLen int
}

// indexEntryInsert inserts one logical index entry (entry ends in a skeyLen-byte storage key). A GIN
// entry term ‖ storage_key joins its term's posting runs (gin.md §4.5); every other entry is its own
// record. false when the entry is already present.
func indexEntryInsert(istore *tableStore, kind indexKind, entry []byte, skeyLen int) (bool, error) {
	if kind == indexGin {
		split := len(entry) - skeyLen
		return ginInsert(istore, entry[:split], entry[split:])
	}
	return istore.Insert(entry, nil)
}

// indexEntryRemove removes one logical index entry (see indexEntryInsert). false when it is absent.
func indexEntryRemove(istore *tableStore, kind indexKind, entry []byte, skeyLen int) (bool, error) {
	if kind == indexGin {
		split := len(entry) - skeyLen
		return ginRemove(istore, entry[:split], entry[split:])
	}
	return istore.Remove(entry)
}

// indexBuildInsert fills an empty index store from its complete entry list, sorted by entry key
// (indexes.md §1): each entry its own record, or for GIN each term's storage keys packed greedily into
// posting runs (gin.md §4.5). Every insert is a right-edge append, so the built tree's shape is a
// function of the entry set — part of the byte contract.
func indexBuildInsert(istore *tableStore, kind indexKind, entries []indexEntry) error {
	dup := func() error { return newError(DataCorrupted, "index entry keys are unique (storage-key suffix)") }
	if kind != indexGin {
		for _, e := range entries {
			ok, err := istore.Insert(e.key, nil)
			if err != nil {
				return err
			}
			if !ok {
				return dup()
			}
		}
		return nil
	}
	for i := 0; i < len(entries); {
		term := entries[i].key[:len(entries[i].key)-entries[i].skeyLen]
		var keys [][]byte
		j := i
		for ; j < len(entries); j++ {
			e := entries[j]
			split := len(e.key) - e.skeyLen
			if !bytes.Equal(e.key[:split], term) {
				break
			}
			keys = append(keys, e.key[split:])
		}
		for _, run := range ginPackRuns(term, keys) {
			ok, err := istore.Insert(run, nil)
			if err != nil {
				return err
			}
			if !ok {
				return dup()
			}
		}
		i = j
	}
	return nil
}

// sortIndexEntries sorts entries by key (then storage-key length), the build order.
func sortIndexEntries(entries []indexEntry) {
	sort.Slice(entries, func(i, j int) bool {
		if c := bytes.Compare(entries[i].key, entries[j].key); c != 0 {
			return c < 0
		}
		return entries[i].skeyLen < entries[j].skeyLen
	})
}
