package jed

import "testing"

// Keywords are legal identifiers in jed (a deliberate PostgreSQL divergence). UPDATE's DEFAULT
// special form must therefore yield to an ordinary expression when the RHS continues.
func TestUpdateDefaultKeywordWithContinuingRHSIsColumn(t *testing.T) {
	stmt, err := parseSQL("UPDATE t SET result = default + 1")
	if err != nil {
		t.Fatal(err)
	}
	a := stmt.Update.Assignments[0]
	if a.IsDefault {
		t.Fatal("continuing RHS was classified as UPDATE DEFAULT")
	}
	if a.Value.Kind != exprBinary || a.Value.Binary.Lhs.Kind != exprColumn || a.Value.Binary.Lhs.Column != "default" {
		t.Fatalf("RHS did not preserve the default column reference: %#v", a.Value)
	}
}
