package jed

// Cross-core time-zone contract: the Go RFC 8536 reader + the JTZ bundle codec must reproduce the
// byte-exact vectors in spec/tz/vectors/{tzif,bundle}.toml (CLAUDE.md §8; spec/tz/README.md §3/§4).
// Mirrors impl/rust/tests/timezone.rs and impl/ts/tests/timezone.test.ts.

import (
	"bytes"
	"fmt"
	"os"
	"testing"
)

func tzBundleBytes(t *testing.T) []byte {
	t.Helper()
	data, err := os.ReadFile(specPath(t, "tz/fixtures/tzdata.jtz"))
	if err != nil {
		t.Fatalf("read tzdata.jtz: %v", err)
	}
	return data
}

func TestTimezoneReaderMatchesVectors(t *testing.T) {
	if err := LoadTimeZoneData(tzBundleBytes(t)); err != nil {
		t.Fatalf("load tzdata.jtz: %v", err)
	}
	rows := readTomlTables(t, specPath(t, "tz/vectors/tzif.toml"), "case")
	if len(rows) == 0 {
		t.Fatal("no tzif vectors")
	}
	for _, row := range rows {
		zone := row.str("zone")
		inst := row.int("instant_micros")
		zr, ok := ResolveZone(zone)
		if !ok {
			t.Fatalf("resolve %s", zone)
		}
		off := offsetAtRef(zr, floorDiv(inst, 1_000_000))
		if int64(off.Utoff) != row.int("utoff_secs") {
			t.Errorf("%s @ %d: utoff = %d, want %d", zone, inst, off.Utoff, row.int("utoff_secs"))
		}
		if off.Abbrev != row.str("abbrev") {
			t.Errorf("%s @ %d: abbrev = %q, want %q", zone, inst, off.Abbrev, row.str("abbrev"))
		}
		if off.IsDst != row.boolVal("is_dst") {
			t.Errorf("%s @ %d: is_dst = %v, want %v", zone, inst, off.IsDst, row.boolVal("is_dst"))
		}
	}
}

func TestTimezoneBundleMatchesVectors(t *testing.T) {
	data := tzBundleBytes(t)
	parsed, err := openTzBundle(data)
	if err != nil {
		t.Fatalf("open tzdata.jtz: %v", err)
	}
	rows := readTomlTables(t, specPath(t, "tz/vectors/bundle.toml"), "bundle")
	if len(rows) != 1 {
		t.Fatalf("want 1 bundle row, got %d", len(rows))
	}
	b := rows[0]

	if parsed.TzdataVersion != b.str("tzdata_version") {
		t.Errorf("tzdata_version = %q, want %q", parsed.TzdataVersion, b.str("tzdata_version"))
	}

	gotZones := make([]string, len(parsed.Zones))
	for i, z := range parsed.Zones {
		gotZones[i] = z.Name
	}
	wantZones := b.strs("zones")
	if fmt.Sprint(gotZones) != fmt.Sprint(wantZones) {
		t.Errorf("zones = %v, want %v", gotZones, wantZones)
	}

	gotLinks := make([]string, len(parsed.Links))
	for i, l := range parsed.Links {
		gotLinks[i] = l.Alias + "=" + l.Target
	}
	wantLinks := b.strs("links")
	if fmt.Sprint(gotLinks) != fmt.Sprint(wantLinks) {
		t.Errorf("links = %v, want %v", gotLinks, wantLinks)
	}

	if !bytes.Equal(saveTzBundle(parsed), data) {
		t.Error("bundle round-trip is not byte-identical")
	}
}
