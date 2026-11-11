package jed

import (
	"slices"
	"testing"
)

// The lane's "volatile" list is the function catalog's (spec/functions/catalog.toml): every function
// it marks volatile, and nothing else. gen_random_uuid is uuidv4's other name.
func TestVolatileScalarFuncsAreTheCatalogs(t *testing.T) {
	var listed []string
	for _, f := range volatileScalarFuncs {
		listed = append(listed, explainScalarFunc(f))
	}
	var catalog []string
	for _, d := range operators {
		if d.Volatility == "volatile" && d.Name != "gen_random_uuid" && !slices.Contains(catalog, d.Name) {
			catalog = append(catalog, d.Name)
		}
	}
	slices.Sort(listed)
	slices.Sort(catalog)
	if !slices.Equal(listed, catalog) {
		t.Fatalf("volatileScalarFuncs %v, the catalog's volatile functions %v", listed, catalog)
	}
}
