package jed

// Host state, persisted dependency lifetime, and argument-sensitive PG admission extensions.
import (
	"bytes"
	"encoding/binary"
	"strings"
	"testing"
)

const zoneIndexSQL = "CREATE UNIQUE INDEX zone_idx ON t (account) WHERE (ts AT TIME ZONE 'America/New_York')::date >= DATE '2024-01-01'"

func buildZoneIndex(t *testing.T) *Session {
	if err := LoadTimeZoneData(tzBundleBytes(t)); err != nil {
		t.Fatal(err)
	}
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t(id int PRIMARY KEY, account int, ts timestamptz)")
	mustExec(t, db, "INSERT INTO t VALUES (1,7,'2024-06-01 00:00:00+00'),(2,7,'2020-01-01 00:00:00+00')")
	mustExec(t, db, zoneIndexSQL)
	return db
}

func skewZonePin(t *testing.T, original []byte, mode string) []byte {
	img := append([]byte(nil), original...)
	tag := []byte("2026a")
	if mode == "missing" {
		tag = []byte("America/New_York")
	}
	start := bytes.LastIndex(img, tag)
	if start < 0 {
		t.Fatal("pin absent")
	}
	off := start + len(tag) - 1
	if mode == "checksum" {
		off++
	}
	name := bytes.LastIndex(img, []byte("America/New_York"))
	switch mode {
	case "mode":
		img[name-5] = 2
	case "empty-static":
		img[name-4] = 0
		img[name-3] = 0
	case "empty-name":
		img[name-2] = 0
		img[name-1] = 0
	case "empty-version":
		img[start-2] = 0
		img[start-1] = 0
	default:
		img[off] ^= 1
	}

	page := start / 4096 * 4096
	covered := append(append([]byte(nil), img[page:page+12]...), img[page+16:page+4096]...)
	binary.BigEndian.PutUint32(img[page+12:page+16], crc32IEEE(covered))
	return img
}

func TestIndexTimezoneLifetime(t *testing.T) {
	image, err := buildZoneIndex(t).ToImage(4096, 1)
	if err != nil {
		t.Fatal(err)
	}
	for _, mode := range []string{"mode", "empty-static", "empty-name", "empty-version"} {
		_, err := loadEngine(skewZonePin(t, image, mode))
		if errCodeOf(err) != "XX001" {
			t.Fatalf("%s: %v", mode, err)
		}
	}
	matching, err := loadEngine(image)
	if err != nil {
		t.Fatal(err)
	}
	if code := errCode(t, matching, "INSERT INTO t VALUES (3,7,'2024-06-01 00:00:00+00')"); code != "23505" {
		t.Fatal(code)
	}
	for _, mode := range []string{"version", "checksum", "missing"} {
		t.Run(mode, func(t *testing.T) {
			db, err := loadEngine(skewZonePin(t, image, mode))
			if err != nil {
				t.Fatal(err)
			}
			for _, row := range query(t, db, "EXPLAIN SELECT id FROM t WHERE (ts AT TIME ZONE 'America/New_York')::date >= DATE '2024-01-01' AND account = 7") {
				for _, v := range row {
					if v.Kind == ValText && strings.Contains(v.str(), "zone_idx") {
						t.Fatal("skewed index used")
					}
				}
			}
			if rows := queryIDs(t, db, "SELECT id FROM t WHERE (ts AT TIME ZONE 'America/New_York')::date >= DATE '2024-01-01' AND account = 7"); len(rows) != 1 || rows[0] != 1 {
				t.Fatal(rows)
			}
			for _, sql := range []string{"INSERT INTO t VALUES (3,8,'2024-06-01 00:00:00+00')", "UPDATE t SET account = 8 WHERE id = 999"} {
				if c := errCode(t, db, sql); c != "XX002" {
					t.Fatal(c)
				}
			}
			mustExec(t, db, "BEGIN")
			mustExec(t, db, "DROP INDEX zone_idx")
			if c := errCode(t, db, "CREATE UNIQUE INDEX zone_idx ON t(account)"); c != "23505" {
				t.Fatal(c)
			}
			mustExec(t, db, "ROLLBACK")
			if c := errCode(t, db, "DELETE FROM t WHERE id = 2"); c != "XX002" {
				t.Fatal(c)
			}
			mustExec(t, db, "BEGIN")
			mustExec(t, db, "DROP INDEX zone_idx")
			mustExec(t, db, zoneIndexSQL)
			mustExec(t, db, "COMMIT")
			mustExec(t, db, "INSERT INTO t VALUES (3,8,'2024-06-01 00:00:00+00')")
			if c := errCode(t, db, "INSERT INTO t VALUES (4,7,'2024-06-01 00:00:00+00')"); c != "23505" {
				t.Fatal(c)
			}
		})
	}
}

func TestIndexTimezoneSessionIndependence(t *testing.T) {
	db := buildZoneIndex(t)
	for _, pred := range []string{"ts IS NULL", "EXTRACT(epoch FROM ts)>0", "date_part('epoch',ts)>0", "make_timestamptz(2024,1,1,0,0,0,'UTC') < ts", "ts + INTERVAL '1 hour' > ts", "uuid_extract_timestamp(uuid '01941f29-7c00-7000-8000-000000000000') IS NOT NULL"} {
		mustExec(t, db, "CREATE INDEX ON t (account) WHERE "+pred)
	}
	for _, zone := range []string{"UTC", "+01", "America/New_York"} {
		if err := db.SetTimeZone(zone); err != nil {
			t.Fatal(err)
		}
		if c := errCode(t, db, "INSERT INTO t VALUES (3,7,'2024-06-01 00:00:00+00')"); c != "23505" {
			t.Fatal(c)
		}
	}
	if c := errCode(t, db, "CREATE INDEX unknown_zone ON t ((ts AT TIME ZONE 'Missing/Zone'))"); c != "22023" {
		t.Fatal(c)
	}
}

func TestIndexDynamicZoneCache(t *testing.T) {
	if err := LoadTimeZoneData(tzBundleBytes(t)); err != nil {
		t.Fatal(err)
	}
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE d (id int PRIMARY KEY, account int, ts timestamptz, zone text)")
	mustExec(t, db, "INSERT INTO d VALUES (1,7,'2024-06-01 00:00:00+00','UTC')")
	ddl := "CREATE INDEX dynamic_idx ON d (account) WHERE (ts AT TIME ZONE zone)::date >= DATE '2024-01-01'"
	mustExec(t, db, ddl)
	stmt, err := db.Prepare("SELECT id FROM d WHERE (ts AT TIME ZONE zone)::date >= DATE '2024-01-01' AND account = 7")
	if err != nil {
		t.Fatal(err)
	}
	drainQ(t, db, stmt)
	drainQ(t, db, stmt)
	if stmt.sc.p.Load() != nil {
		t.Fatal("dynamic-zone read was cached")
	}
	ins, err := db.Prepare("INSERT INTO d VALUES ($1,7,'2024-06-01 00:00:00+00','UTC') RETURNING id")
	if err != nil {
		t.Fatal(err)
	}
	drainQ(t, db, ins, IntValue(90))
	drainQ(t, db, ins, IntValue(91))
	if ins.ic.p.Load() != nil {
		t.Fatal("dynamic-zone insert was cached")
	}
	b, err := openTzBundle(tzBundleBytes(t))
	if err != nil {
		t.Fatal(err)
	}
	b.Zones = []tzZoneSection{{Name: "Test/IndexAdded", Raw: b.Zones[0].Raw}}
	b.Links = nil
	if err := LoadTimeZoneData(saveTzBundle(b)); err != nil {
		t.Fatal(err)
	}
	drainQ(t, db, stmt)
	rows, err := db.queryStmt(ins.ast, []Value{IntValue(92)}, &ins.sc, &ins.ic)
	if err == nil {
		for rows.Next() {
		}
		err = rows.Err()
		rows.Close()
	}
	if errCodeOf(err) != "XX002" {
		t.Fatalf("prepared insert after load: %v", err)
	}
	if c := errCode(t, db, "INSERT INTO d VALUES (2,7,'2024-06-01 00:00:00+00','UTC')"); c != "XX002" {
		t.Fatal(c)
	}
	mustExec(t, db, "BEGIN")
	mustExec(t, db, "DROP INDEX dynamic_idx")
	mustExec(t, db, ddl)
	mustExec(t, db, "COMMIT")
	mustExec(t, db, "INSERT INTO d VALUES (2,7,'2024-06-01 00:00:00+00','UTC')")
}
