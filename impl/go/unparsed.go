package jed

import "errors"

// Stored definitions this binary cannot parse (spec/design/unparsable-definitions.md). The catalog
// load keeps a view, CHECK, expression DEFAULT, or expression/partial index whose stored text the
// parser rejects, marked Unparsed with its text verbatim. Only a statement that needs the parsed form
// fails, with XX003, before it reads or writes a row.

func unparsableError(detail string) error {
	return newError(UnparsableStoredDefinition, "stored definition does not parse in this version of jed: "+detail)
}

func unparsableViewError(name string) error {
	return unparsableError("view \"" + name + "\"")
}

func unparsableCheckError(table, check string) error {
	return unparsableError("check constraint \"" + check + "\" of table \"" + table + "\"")
}

func unparsableDefaultError(table, column string) error {
	return unparsableError("default of column \"" + column + "\" of table \"" + table + "\"")
}

func unparsableIndexError(table, index string) error {
	return unparsableError("index \"" + index + "\" of table \"" + table + "\"")
}

// unparsed reports whether an expression key or the predicate of the index did not parse (§3.4).
func (d *indexDef) unparsed() bool {
	for _, k := range d.Keys {
		if k.Expr != nil && k.Expr.Unparsed {
			return true
		}
	}
	return d.Predicate != nil && d.Predicate.Unparsed
}

// tableUnparsedError is XX003 for the first unparsed check (name order), expression default (column
// order, when defaults), or index (catalog order) of t, else nil. The ALTER TABLE actions that analyze
// or rewrite every stored expression of a table call it before they change anything (§3.2–§3.4); DROP
// COLUMN passes defaults = false, since a default cannot reference a column.
func tableUnparsedError(t *catTable, defaults bool) error {
	for i := range t.Checks {
		if t.Checks[i].Unparsed {
			return unparsableCheckError(t.Name, t.Checks[i].Name)
		}
	}
	for i := range t.Columns {
		if de := t.Columns[i].DefaultExpr; defaults && de != nil && de.Unparsed {
			return unparsableDefaultError(t.Name, t.Columns[i].Name)
		}
	}
	for i := range t.Indexes {
		if t.Indexes[i].unparsed() {
			return unparsableIndexError(t.Name, t.Indexes[i].Name)
		}
	}
	return nil
}

// isUnparsableError reports whether err is XX003. View re-validation reports a dependent that fails
// to plan as the generic 0A000; an unparsed view reached while planning it passes through instead, so
// the error names the view that needs a newer jed.
func isUnparsableError(err error) bool {
	var e *EngineError
	return errors.As(err, &e) && e.State == UnparsableStoredDefinition
}
