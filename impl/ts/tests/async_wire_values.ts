import { Decimal } from "../src/decimal.ts";
import type { Value } from "../src/value.ts";

export const decimal = Decimal.fromDigitsScale(true, "12345678901234567890123456789000100", 5);
export const values: Value[] = [
  { kind: "null" },
  { kind: "int", int: -9223372036854775808n },
  { kind: "int", int: 9223372036854775807n },
  { kind: "bool", value: false },
  { kind: "timestamp", micros: -9223372036854775808n },
  { kind: "timestamptz", micros: 9223372036854775807n },
  { kind: "date", days: -2147483648n },
  { kind: "interval", iv: { months: -12, days: 32, micros: 9007199254740993n } },
  { kind: "text", text: "𝄞 café\u0000\ud800" },
  { kind: "decimal", dec: decimal },
  { kind: "f32", value: Math.fround(1.23456789) },
  { kind: "f64", value: Number.MIN_VALUE },
  { kind: "f64", value: -0 },
  { kind: "f64", value: NaN },
  { kind: "f64", value: Infinity },
  { kind: "f64", value: -Infinity },
  { kind: "bytea", bytes: Uint8Array.from([0, 255, 1, 128]) },
  { kind: "uuid", bytes: Uint8Array.from({ length: 16 }, (_, i) => i) },
  { kind: "composite", fields: [{ kind: "decimal", dec: decimal }, { kind: "null" }] },
  {
    kind: "array",
    dims: [2],
    lbounds: [-4],
    elements: [{ kind: "decimal", dec: decimal }, { kind: "null" }],
  },
  { kind: "array", dims: [], lbounds: [], elements: [] },
  {
    kind: "range",
    empty: false,
    lower: { kind: "decimal", dec: decimal },
    upper: null,
    lowerInc: true,
    upperInc: false,
  },
  { kind: "range", empty: true, lower: null, upper: null, lowerInc: false, upperInc: false },
  { kind: "json", text: '{ "n": 1.00, "n": 2 }' },
  { kind: "jsonpath", text: "strict $.a[*]" },
  {
    kind: "jsonb",
    node: {
      kind: "object",
      members: [
        { key: "n", value: { kind: "number", dec: decimal } },
        {
          key: "x",
          value: {
            kind: "array",
            elements: [
              { kind: "null" },
              { kind: "bool", value: true },
              { kind: "string", value: "雪" },
            ],
          },
        },
      ],
    },
  },
];
