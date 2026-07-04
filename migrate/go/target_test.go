package migrate

import (
	"reflect"
	"testing"
)

func TestResolveTargets(t *testing.T) {
	const n = 5
	cases := []struct {
		spec    string
		current int
		want    []int
		wantErr bool
	}{
		{spec: "", current: 0, want: []int{5}},
		{spec: "last", current: 2, want: []int{5}},
		{spec: "3", current: 0, want: []int{3}},
		{spec: "0", current: 5, want: []int{0}},
		{spec: "+2", current: 1, want: []int{3}},
		{spec: "-2", current: 5, want: []int{3}},
		{spec: "-+1", current: 5, want: []int{4, 5}}, // redo the last one
		{spec: "-+3", current: 5, want: []int{2, 5}},
		// out of range
		{spec: "6", current: 0, wantErr: true},
		{spec: "-1", current: 0, wantErr: true},
		{spec: "+9", current: 0, wantErr: true},
		{spec: "-+9", current: 5, wantErr: true},
		{spec: "banana", current: 0, wantErr: true},
		{spec: "+", current: 0, wantErr: true},
	}
	for _, c := range cases {
		got, err := ResolveTargets(c.spec, c.current, n)
		if c.wantErr {
			if err == nil {
				t.Errorf("ResolveTargets(%q, %d) = %v; want error", c.spec, c.current, got)
			}
			continue
		}
		if err != nil {
			t.Errorf("ResolveTargets(%q, %d): %v", c.spec, c.current, err)
			continue
		}
		if !reflect.DeepEqual(got, c.want) {
			t.Errorf("ResolveTargets(%q, %d) = %v; want %v", c.spec, c.current, got, c.want)
		}
	}
}
