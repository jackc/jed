package jed

import (
	"bytes"
	"testing"
)

// inlineBodySpanMatchesDecode is the L1 cross-check (spec/design/lazy-record.md §6/§12): the
// no-construct skip walk advances the cursor identically to the eager construct decode, for EVERY
// value type (scalars incl. text/bytea/decimal/json/jsonb, and the array/composite/range
// containers). inlineBodySpan must land *pos at exactly the position readInlineBody(decodeConstruct)
// reaches and return precisely the body bytes — the zero-drift property the lazy-record reshape rests
// on. A construct decode of those same bytes must also still re-encode to the original (the eager
// path is unchanged).
func TestInlineBodySpanMatchesDecode(t *testing.T) {
	t.Parallel()
	i32 := scalarColType(scalarInt32)
	text := scalarColType(scalarText)
	field := func(name string, ty colType) colField {
		return colField{Name: name, Type: ty}
	}

	// A jsonb document touching every node kind: object, nested array, number, string, bool, null,
	// and an empty string.
	doc := JsonNode{Kind: JObject, Obj: []JsonMember{
		{Key: "a", Val: JsonNode{Kind: JNumber, Num: decimalFromDigitsScale(false, "1234", 2)}},
		{Key: "b", Val: JsonNode{Kind: JArray, Arr: []JsonNode{
			{Kind: JBool, B: true},
			{Kind: JNull},
			{Kind: JString, S: "x"},
		}}},
		{Key: "c", Val: JsonNode{Kind: JString, S: ""}},
	}}

	compTy := colType{Composite: true, Name: "pair", Fields: []colField{
		field("a", i32),
		field("b", text),
	}}
	rangeTy := colType{RangeElem: &i32}
	arrI32 := colType{Elem: &i32}
	arrText := colType{Elem: &text}

	cases := []struct {
		ty colType
		v  Value
	}{
		{scalarColType(scalarInt16), IntValue(-12345)},
		{i32, IntValue(70000)},
		{scalarColType(scalarInt64), IntValue(-9223372036854775808)},
		{text, TextValue("hello, jed")},
		{text, TextValue("")}, // empty text
		{scalarColType(scalarBool), BoolValue(true)},
		{scalarColType(scalarBool), BoolValue(false)},
		{scalarColType(scalarDecimal), DecimalValue(decimalFromDigitsScale(true, "9876543210", 4))},
		{scalarColType(scalarBytea), ByteaValue([]byte{0, 1, 2, 255, 254})},
		{scalarColType(scalarUuid), UuidValue(bytes.Repeat([]byte{7}, 16))},
		{scalarColType(scalarTimestamp), TimestampValue(1700000000000000)},
		{scalarColType(scalarTimestamptz), TimestamptzValue(-42)},
		{scalarColType(scalarDate), DateValue(-19000)},
		{scalarColType(scalarInterval), IntervalValue(Interval{Months: 14, Days: -3, Micros: 123456})},
		{scalarColType(scalarFloat64), Float64Value(3.141592653589793)},
		{scalarColType(scalarFloat32), Float32Value(-2.5)},
		{scalarColType(scalarJson), JsonValue(`{"k": 1}`)},
		{scalarColType(scalarJsonb), JsonbValue(doc)},
		// Array of i32 with a NULL element (exercises the has-nulls bitmap branch).
		{arrI32, arrayValueOf(oneDimArray([]Value{IntValue(1), NullValue(), IntValue(3)}))},
		// Array of text (variable-length elements recurse through readInlineBody).
		{arrText, arrayValueOf(oneDimArray([]Value{TextValue("a"), TextValue("bb")}))},
		// Empty array (ndim 0 short-circuit).
		{arrI32, arrayValueOf(emptyArray())},
		// Composite with a present field and (next) a NULL field.
		{compTy, CompositeValue([]Value{IntValue(5), TextValue("hi")})},
		{compTy, CompositeValue([]Value{NullValue(), TextValue("only b")})},
		// Range: bounded [1,5), the empty range, and unbounded-below (-inf,9).
		{rangeTy, RangeValue(&RangeVal{Lower: ptrVal(IntValue(1)), Upper: ptrVal(IntValue(5)), LowerInc: true})},
		{rangeTy, RangeValue(emptyRangeVal())},
		{rangeTy, RangeValue(&RangeVal{Upper: ptrVal(IntValue(9))})},
	}

	for _, c := range cases {
		enc := encodeValue(c.ty, c.v)
		if enc[0] != 0x00 {
			t.Fatalf("present values carry the 0x00 tag, got %#x for %v", enc[0], c.v)
		}

		// Construct decode: consumes the whole body and re-encodes to the original bytes.
		pc := 1
		got, err := readInlineBody(c.ty, enc, &pc, decodeConstruct)
		if err != nil {
			t.Fatalf("construct decode %v: %v", c.v, err)
		}
		if pc != len(enc) {
			t.Fatalf("construct decode of %v consumed %d, want %d", c.v, pc, len(enc))
		}
		if reenc := encodeValue(c.ty, got); !bytes.Equal(reenc, enc) {
			t.Fatalf("construct decode of %v did not round-trip: re-encoded %x != %x", c.v, reenc, enc)
		}

		// Skip walk: lands at the identical cursor and returns exactly the body bytes — no value
		// constructed.
		ps := 1
		span, err := inlineBodySpan(c.ty, enc, &ps)
		if err != nil {
			t.Fatalf("skip walk %v: %v", c.v, err)
		}
		if ps != pc {
			t.Fatalf("skip advance %d != construct advance %d for %v", ps, pc, c.v)
		}
		if !bytes.Equal(span, enc[1:]) {
			t.Fatalf("span %x != body bytes %x for %v", span, enc[1:], c.v)
		}
	}
}

// ptrVal returns a pointer to a Value (range bounds are *Value).
func ptrVal(v Value) *Value { return &v }
