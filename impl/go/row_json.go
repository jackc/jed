package jed

// row_to_json(record [, pretty]) (mirrors impl/rust executor/row_json.rs): a port of PostgreSQL's
// composite_to_json / array_to_json_internal / datum_to_json_internal (json.c) — the composite →
// json object render (spec/design/json-sql-functions.md §2.2).

import (
	"strconv"
	"strings"
)

// compositeToJSON renders the composite value fields, of the composite type ty, as a json object
// onto out: {"name":value,…} in declaration order. pretty separates the members with ",\n " instead
// of "," (PostgreSQL's use_line_feeds, honored at the top level only). A timestamptz at any depth
// renders in zone.
func compositeToJSON(fields []Value, ty *compositeRType, pretty bool, zone ZoneRef, out *strings.Builder) error {
	sep := ","
	if pretty {
		sep = ",\n "
	}
	out.WriteByte('{')
	for i, f := range ty.fields {
		if i > 0 {
			out.WriteString(sep)
		}
		writeJSONString(f.name, out)
		out.WriteByte(':')
		if err := datumToJSON(fields[i], f.ty, zone, out); err != nil {
			return err
		}
	}
	out.WriteByte('}')
	return nil
}

// datumToJSON writes one value's JSON image (PostgreSQL's datum_to_json_internal, never a key). ty
// is the value's static type, read only for the field names of a composite (directly or as an array
// element).
func datumToJSON(v Value, ty resolvedType, zone ZoneRef, out *strings.Builder) error {
	switch v.Kind {
	case ValNull:
		out.WriteString("null")
	case ValBool:
		if v.boolVal() {
			out.WriteString("true")
		} else {
			out.WriteString("false")
		}
	case ValInt:
		// A number is its exact text (jed's decimal is always finite, so never a quoted NaN).
		out.WriteString(strconv.FormatInt(v.Int, 10))
	case ValDecimal:
		out.WriteString(v.decimal().Render())
	case ValText:
		writeJSONString(v.str(), out)
	case ValJson:
		// json embeds its text verbatim; jsonb its canonical (spaced) output.
		out.WriteString(v.str())
	case ValJsonb:
		out.WriteString(jsonbOut(v.jsonb()))
	case ValComposite:
		return compositeToJSON(*v.composite(), ty.comp, false, zone, out)
	case ValArray:
		return arrayToJSON(v.arrayVal(), *ty.elem, zone, out)
	// The datetimes take PostgreSQL's JsonEncodeDateTime (XSD) form.
	case ValDate:
		writeJSONString(renderDate(int32(v.Int)), out)
	case ValTimestamp:
		writeJSONString(renderTimestampXSD(v.Int), out)
	case ValTimestamptz:
		text, err := renderTimestamptzXSDIn(v.Int, zone)
		if err != nil {
			return err
		}
		writeJSONString(text, out)
	case ValFloat32, ValFloat64:
		return newError(FeatureNotSupported, "row_to_json of a float value is not supported yet")
	case ValInterval, ValUuid, ValBytea, ValRange, ValJsonPath:
		// Every other type is the JSON string of its text output (a range's timestamptz bounds in
		// the session zone).
		text, err := v.renderIn(zone)
		if err != nil {
			return err
		}
		writeJSONString(text, out)
	default:
		panic("BUG: unfetched large value escaped the storage layer")
	}
	return nil
}

// arrayToJSON writes an array's JSON image (PostgreSQL's array_to_json_internal without line feeds):
// one bracket level per dimension, the lower bounds dropped; an empty array is [].
func arrayToJSON(a *ArrayVal, elem resolvedType, zone ZoneRef, out *strings.Builder) error {
	if len(a.Elements) == 0 {
		out.WriteString("[]")
		return nil
	}
	next := 0
	return arrayDimToJSON(a, 0, elem, zone, &next, out)
}

// arrayDimToJSON writes one dimension of arrayToJSON: the innermost dimension renders Dims[dim]
// elements from next, an outer one recurses once per sub-array.
func arrayDimToJSON(a *ArrayVal, dim int, elem resolvedType, zone ZoneRef, next *int, out *strings.Builder) error {
	out.WriteByte('[')
	for i := 0; i < a.Dims[dim]; i++ {
		if i > 0 {
			out.WriteByte(',')
		}
		if dim+1 == a.Ndim() {
			if err := datumToJSON(a.Elements[*next], elem, zone, out); err != nil {
				return err
			}
			*next++
		} else if err := arrayDimToJSON(a, dim+1, elem, zone, next, out); err != nil {
			return err
		}
	}
	out.WriteByte(']')
	return nil
}
