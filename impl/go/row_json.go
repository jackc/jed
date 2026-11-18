package jed

// The JSON image of a value (mirrors impl/rust executor/row_json.rs): a port of PostgreSQL's json.c
// datum_to_json_internal / array_to_json_internal / composite_to_json and jsonb.c
// datum_to_jsonb_internal / array_to_jsonb_internal / composite_to_jsonb. to_json, to_jsonb,
// array_to_json, row_to_json, the json[b] builders, and the json[b] aggregates all convert their
// arguments here (spec/design/json-sql-functions.md §2.2).

import (
	"math"
	"strconv"
	"strings"
)

// compositeToJSON renders the composite value fields, of the composite type ty, as a json object
// onto out: {"name":value,…} in declaration order. pretty separates the members with ",\n " instead
// of "," (PostgreSQL's use_line_feeds, honored at the top level only). A timestamptz at any depth
// renders in zone.
func compositeToJSON(fields []Value, ty *compositeRType, pretty bool, zone zoneRef, out *strings.Builder) error {
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

// datumToJSON writes one value's json image (PostgreSQL's datum_to_json_internal, never a key). ty
// is the value's static type, read only for the field names of a composite (directly or as an array
// element).
func datumToJSON(v Value, ty resolvedType, zone zoneRef, out *strings.Builder) error {
	switch v.kind {
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
		out.WriteString(strconv.FormatInt(v.num, 10))
	case ValDecimal:
		out.WriteString(v.decimal().String())
	case ValText:
		writeJSONString(v.str(), out)
	case ValJSON:
		// json embeds its text verbatim; jsonb its canonical (spaced) output.
		out.WriteString(v.str())
	case ValJSONB:
		out.WriteString(jsonbOut(v.jsonb()))
	case ValComposite:
		return compositeToJSON(*v.composite(), ty.comp, false, zone, out)
	case ValArray:
		return arrayToJSON(v.arrayVal(), *ty.elem, false, zone, out)
	// The datetimes take PostgreSQL's JsonEncodeDateTime (XSD) form.
	case ValDate:
		writeJSONString(renderDate(int32(v.num)), out)
	case ValTimestamp:
		writeJSONString(renderTimestampXSD(v.num), out)
	case ValTimestamptz:
		text, err := renderTimestamptzXSDIn(v.num, zone)
		if err != nil {
			return err
		}
		writeJSONString(text, out)
	case ValFloat32, ValFloat64:
		// PostgreSQL's float8out/float4out text: a number, or the JSON string of NaN/±Infinity.
		f, bits := floatOf(v)
		if math.IsNaN(f) || math.IsInf(f, 0) {
			writeJSONString(renderFloatPG(f, bits), out)
		} else {
			out.WriteString(renderFloatPG(f, bits))
		}
	case ValInterval, ValUUID, ValBytea, ValRange, ValJSONPath:
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

// arrayToJSON writes an array's json image (PostgreSQL's array_to_json_internal): one bracket level
// per dimension, the lower bounds dropped; an empty array is []. pretty separates the OUTERMOST
// dimension's items with ",\n " (inner dimensions and the elements never break lines).
func arrayToJSON(a *ArrayVal, elem resolvedType, pretty bool, zone zoneRef, out *strings.Builder) error {
	if len(a.Elements) == 0 {
		out.WriteString("[]")
		return nil
	}
	next := 0
	return arrayDimToJSON(a, 0, elem, pretty, zone, &next, out)
}

// arrayDimToJSON writes one dimension of arrayToJSON: the innermost dimension renders Dims[dim]
// elements from next, an outer one recurses once per sub-array (never with line feeds).
func arrayDimToJSON(a *ArrayVal, dim int, elem resolvedType, pretty bool, zone zoneRef, next *int, out *strings.Builder) error {
	sep := ","
	if pretty {
		sep = ",\n "
	}
	out.WriteByte('[')
	for i := 0; i < a.Dims[dim]; i++ {
		if i > 0 {
			out.WriteString(sep)
		}
		if dim+1 == a.Ndim() {
			if err := datumToJSON(a.Elements[*next], elem, zone, out); err != nil {
				return err
			}
			*next++
		} else if err := arrayDimToJSON(a, dim+1, elem, false, zone, next, out); err != nil {
			return err
		}
	}
	out.WriteByte(']')
	return nil
}

// datumToJSONB is one value's jsonb image (PostgreSQL's datum_to_jsonb_internal, never a key): as
// datumToJSON, except that a json value is parsed (duplicate keys collapse, last wins), a composite
// is an object in canonical key order, and the result is a node, not text.
func datumToJSONB(v Value, ty resolvedType, zone zoneRef) (JSONNode, error) {
	switch v.kind {
	case ValNull:
		return JSONNode{Kind: JNull}, nil
	case ValBool:
		return JSONNode{Kind: JBool, B: v.boolVal()}, nil
	case ValInt:
		return JSONNode{Kind: JNumber, Num: decimalFromInt64(v.num)}, nil
	case ValDecimal:
		return JSONNode{Kind: JNumber, Num: *v.decimal()}, nil
	case ValText:
		return JSONNode{Kind: JString, S: v.str()}, nil
	case ValJSON:
		return jsonbIn(v.str())
	case ValJSONB:
		return *v.jsonb(), nil
	case ValComposite:
		fields := *v.composite()
		members := make([]JSONMember, 0, len(fields))
		for i, f := range ty.comp.fields {
			node, err := datumToJSONB(fields[i], f.ty, zone)
			if err != nil {
				return JSONNode{}, err
			}
			members = append(members, JSONMember{Key: f.name, Val: node})
		}
		return makeObject(members), nil
	case ValArray:
		a := v.arrayVal()
		if len(a.Elements) == 0 {
			return JSONNode{Kind: JArray, Arr: []JSONNode{}}, nil
		}
		next := 0
		return arrayDimToJSONB(a, 0, *ty.elem, zone, &next)
	case ValDate:
		return JSONNode{Kind: JString, S: renderDate(int32(v.num))}, nil
	case ValTimestamp:
		return JSONNode{Kind: JString, S: renderTimestampXSD(v.num)}, nil
	case ValTimestamptz:
		text, err := renderTimestamptzXSDIn(v.num, zone)
		if err != nil {
			return JSONNode{}, err
		}
		return JSONNode{Kind: JString, S: text}, nil
	case ValFloat32, ValFloat64:
		// The number numeric_in reads from the float8out/float4out text; NaN/±Infinity are strings.
		f, bits := floatOf(v)
		if math.IsNaN(f) || math.IsInf(f, 0) {
			return JSONNode{Kind: JString, S: renderFloatPG(f, bits)}, nil
		}
		return JSONNode{Kind: JNumber, Num: floatJSONNumber(f, bits)}, nil
	case ValInterval, ValUUID, ValBytea, ValRange, ValJSONPath:
		text, err := v.renderIn(zone)
		if err != nil {
			return JSONNode{}, err
		}
		return JSONNode{Kind: JString, S: text}, nil
	default:
		panic("BUG: unfetched large value escaped the storage layer")
	}
}

// arrayDimToJSONB is one dimension of an array's jsonb image (PostgreSQL's array_dim_to_jsonb).
func arrayDimToJSONB(a *ArrayVal, dim int, elem resolvedType, zone zoneRef, next *int) (JSONNode, error) {
	items := make([]JSONNode, 0, a.Dims[dim])
	for i := 0; i < a.Dims[dim]; i++ {
		var node JSONNode
		var err error
		if dim+1 == a.Ndim() {
			node, err = datumToJSONB(a.Elements[*next], elem, zone)
			*next++
		} else {
			node, err = arrayDimToJSONB(a, dim+1, elem, zone, next)
		}
		if err != nil {
			return JSONNode{}, err
		}
		items = append(items, node)
	}
	return JSONNode{Kind: JArray, Arr: items}, nil
}

// jsonKeyText is the text of an object KEY (PostgreSQL's datum_to_json[b]_internal with key_scalar):
// a key is always a JSON string. A number or boolean is its text, a datetime its ISO 8601 form, any
// other scalar its text output. An array, composite, json, or jsonb key is 22023. The caller rejects
// a NULL key first.
func jsonKeyText(v Value, zone zoneRef) (string, error) {
	switch v.kind {
	case ValArray, ValComposite, ValJSON, ValJSONB:
		return "", newError(InvalidParameterValue, "key value must be scalar, not array, composite, or json")
	case ValBool:
		if v.boolVal() {
			return "true", nil
		}
		return "false", nil
	case ValInt:
		return strconv.FormatInt(v.num, 10), nil
	case ValDecimal:
		return v.decimal().String(), nil
	case ValText:
		return v.str(), nil
	case ValDate:
		return renderDate(int32(v.num)), nil
	case ValTimestamp:
		return renderTimestampXSD(v.num), nil
	case ValTimestamptz:
		return renderTimestamptzXSDIn(v.num, zone)
	case ValFloat32, ValFloat64:
		f, bits := floatOf(v)
		return renderFloatPG(f, bits), nil
	case ValInterval, ValUUID, ValBytea, ValRange, ValJSONPath:
		return v.renderIn(zone)
	case ValNull:
		panic("jsonKeyText: the caller rejects a NULL key")
	default:
		panic("BUG: unfetched large value escaped the storage layer")
	}
}

// jsonAggBreaksLine reports whether json_agg separates a value of type ty from the previous element
// with an extra "\n " — PostgreSQL does so for the array and composite categories.
func jsonAggBreaksLine(ty resolvedType) bool {
	return ty.kind == rtArray || ty.kind == rtComposite
}
