package jed

import (
	"context"
	"testing"
)

// Value.Scan (host-API surface): the Rows.Scan conversions plus the container types, each a copy.
func TestValueScan(t *testing.T) {
	var n int64
	if err := IntValue(7).Scan(&n); err != nil || n != 7 {
		t.Fatalf("int: %d %v", n, err)
	}
	var s string
	if err := TextValue("a").Scan(&s); err != nil || s != "a" {
		t.Fatalf("text: %q %v", s, err)
	}
	if err := NullValue().Scan(&s); err == nil {
		t.Fatal("NULL into *string accepted")
	}
	var ns Null[string]
	if err := NullValue().Scan(&ns); err != nil || ns.Valid {
		t.Fatalf("NULL into Null[string]: %+v %v", ns, err)
	}

	db := mustDB(t)
	row := db.QueryRow(context.Background(), "SELECT ARRAY[1,2,3], int4range(1, 5), '{\"a\": [1]}'::jsonb, ROW(1, 'x')")
	var arr, rng, doc, comp Value
	if err := row.Scan(&arr, &rng, &doc, &comp); err != nil {
		t.Fatal(err)
	}
	var a ArrayVal
	if err := arr.Scan(&a); err != nil || len(a.Elements) != 3 {
		t.Fatalf("array: %+v %v", a, err)
	}
	a.Elements[0] = IntValue(99) // a copy: the Value is unchanged
	if arr.String() != "{1,2,3}" {
		t.Fatalf("scan aliased the array: %s", arr.String())
	}
	var r RangeVal
	if err := rng.Scan(&r); err != nil || r.Empty || r.Lower == nil {
		t.Fatalf("range: %+v %v", r, err)
	}
	var j JSONNode
	if err := doc.Scan(&j); err != nil || j.Kind != JObject || len(j.Obj) != 1 {
		t.Fatalf("jsonb: %+v %v", j, err)
	}
	j.Obj[0].Val.Arr[0] = JSONNode{Kind: JNull}
	if doc.String() != `{"a": [1]}` {
		t.Fatalf("scan aliased the jsonb tree: %s", doc.String())
	}
	var fields []Value
	if err := comp.Scan(&fields); err != nil || len(fields) != 2 {
		t.Fatalf("composite: %v %v", fields, err)
	}
	if err := arr.Scan(&r); err == nil {
		t.Fatal("array into *RangeVal accepted")
	}
}
