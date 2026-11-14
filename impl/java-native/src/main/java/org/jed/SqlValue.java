package org.jed;

import java.util.Arrays;
import java.util.Objects;

/** Typed value with an unboxed primitive payload. SQL NULL is explicit and retains its type. */
public final class SqlValue {
    private final SqlType type;
    private final boolean isNull;
    private final long bits;
    private final Object payload;

    private SqlValue(SqlType type, boolean isNull, long bits, Object payload) {
        this.type = Objects.requireNonNull(type, "type");
        this.isNull = isNull;
        this.bits = bits;
        this.payload = payload;
    }

    public static SqlValue nullValue(SqlType type) { return new SqlValue(type, true, 0, null); }
    public static SqlValue ofBoolean(boolean value) { return new SqlValue(SqlType.BOOLEAN, false, value ? 1 : 0, null); }
    public static SqlValue ofInteger(SqlType type, long value) {
        if (!type.isInteger()) throw SqlException.typeMismatch("expected integer type");
        if (!type.contains(value)) throw SqlException.numericOverflow();
        return new SqlValue(type, false, value, null);
    }
    public static SqlValue ofInteger(long value) { return ofInteger(SqlType.I64, value); }
    public static SqlValue ofText(String value) { return new SqlValue(SqlType.TEXT, false, 0, SqlText.validate(value)); }
    public static SqlValue ofText(SqlType type, String value) {
        if (type.kind() != SqlType.Kind.TEXT) throw SqlException.typeMismatch("expected text type");
        SqlText.validate(value);
        if (type.maxLength() >= 0 && SqlText.length(value) > type.maxLength()) {
            throw new SqlException("22001", "value too long for " + type);
        }
        return new SqlValue(type, false, 0, value);
    }
    public static SqlValue ofDecimal(Decimal value) {
        return new SqlValue(SqlType.DECIMAL, false, 0, Objects.requireNonNull(value, "value").checked());
    }
    public static SqlValue ofDecimal(SqlType type, Decimal value) {
        if (type.kind() != SqlType.Kind.DECIMAL) throw SqlException.typeMismatch("expected decimal type");
        return new SqlValue(type, false, 0,
            type.precision() < 0 ? value.checked() : value.constrain(type.precision(), type.scale()));
    }
    public static SqlValue ofFloat(SqlType type, double value) {
        if (!type.isFloat()) throw SqlException.typeMismatch("expected float type");
        if (type.kind() == SqlType.Kind.F32) {
            float narrowed = (float) value;
            if (Double.isFinite(value) && (!Float.isFinite(narrowed) || (value != 0 && narrowed == 0))) {
                throw SqlException.numericOverflow();
            }
            value = narrowed;
        }
        return new SqlValue(type, false, Double.doubleToRawLongBits(value), null);
    }
    public static SqlValue ofBytes(byte[] value) {
        return new SqlValue(SqlType.BYTEA, false, 0, value.clone());
    }

    public static SqlValue ofDate(int days) { return new SqlValue(SqlType.DATE, false, days, null); }
    public int asDate() { require(SqlType.Kind.DATE); return (int) bits; }
    public static SqlValue ofTimestamp(SqlType type, long micros) {
        if (type.kind() != SqlType.Kind.TIMESTAMP && type.kind() != SqlType.Kind.TIMESTAMPTZ) throw SqlException.typeMismatch("expected timestamp type");
        return new SqlValue(type, false, type.precision() < 0 ? micros : Temporal.roundPrecision(micros, type.precision()), null);
    }
    public long asTimestamp() {
        if (isNull) throw new SqlException("22004", "SQL NULL has no payload");
        if (type.kind() != SqlType.Kind.TIMESTAMP && type.kind() != SqlType.Kind.TIMESTAMPTZ) throw SqlException.typeMismatch("expected timestamp type");
        return bits;
    }
    public static SqlValue ofInterval(SqlInterval value) { return new SqlValue(SqlType.INTERVAL, false, 0, Objects.requireNonNull(value, "value")); }
    public static SqlValue ofInterval(SqlType type, SqlInterval value) {
        if (type.kind() != SqlType.Kind.INTERVAL) throw SqlException.typeMismatch("expected interval type");
        return new SqlValue(type, false, 0, value.constrain(type));
    }
    public SqlInterval asInterval() { require(SqlType.Kind.INTERVAL); return (SqlInterval) payload; }
    public static SqlValue ofUuid(SqlUuid value) { return new SqlValue(SqlType.UUID, false, 0, Objects.requireNonNull(value, "value")); }
    public SqlUuid asUuid() { require(SqlType.Kind.UUID); return (SqlUuid) payload; }

    public static SqlValue ofArray(SqlType type, SqlArray value) {
        if (type.kind() != SqlType.Kind.ARRAY) throw SqlException.typeMismatch("expected array type");
        for (SqlValue element : value.elements()) if (!element.type().equals(type.element())) throw SqlException.typeMismatch("array element type mismatch");
        return new SqlValue(type, false, 0, value);
    }
    public SqlArray asArray() { require(SqlType.Kind.ARRAY); return (SqlArray) payload; }

    public static SqlValue ofJsonPath(JsonPath path) { return new SqlValue(SqlType.JSONPATH, false, 0, java.util.Objects.requireNonNull(path)); }
    public JsonPath asJsonPath() { require(SqlType.Kind.JSONPATH); return (JsonPath) payload; }
    public static SqlValue ofJson(String text) { JsonNode.parse(text, false); return new SqlValue(SqlType.JSON, false, 0, text); }
    public String asJson() { require(SqlType.Kind.JSON); return (String) payload; }
    public static SqlValue ofJsonb(JsonNode node) { node.checkDepth(); return new SqlValue(SqlType.JSONB, false, 0, node.canonical()); }
    public JsonNode asJsonb() { require(SqlType.Kind.JSONB); return (JsonNode) payload; }

    public static SqlValue ofRange(SqlType type, SqlRange value) {
        if (type.kind() != SqlType.Kind.RANGE) throw SqlException.typeMismatch("expected range type");
        if (value.lower() != null && !value.lower().type().equals(type.element()) || value.upper() != null && !value.upper().type().equals(type.element())) throw SqlException.typeMismatch("range endpoint type mismatch");
        return new SqlValue(type, false, 0, Objects.requireNonNull(value, "value"));
    }
    public SqlRange asRange() { require(SqlType.Kind.RANGE); return (SqlRange) payload; }

    public static SqlValue ofComposite(SqlType type, java.util.List<SqlValue> fields) {
        if (!type.isComposite() || type.fields().size() != fields.size()) throw SqlException.typeMismatch("composite field count mismatch");
        for (int i = 0; i < fields.size(); i++) {
            if (!fields.get(i).type().equals(type.fields().get(i).type())) throw SqlException.typeMismatch("composite field type mismatch");
            if (fields.get(i).isNull() && type.fields().get(i).notNull()) throw new SqlException("23502", "NULL composite field");
        }
        return new SqlValue(type, false, 0, java.util.List.copyOf(fields));
    }
    @SuppressWarnings("unchecked")
    public java.util.List<SqlValue> asComposite() {
        if (isNull) throw new SqlException("22004", "SQL NULL has no payload");
        if (!type.isComposite()) throw SqlException.typeMismatch("expected composite type");
        return (java.util.List<SqlValue>) payload;
    }

    /** Binary-coercible output descriptors may carry unenforced scalar typmods. */
    SqlValue relabel(SqlType target) {
        if (type.kind() != target.kind()) throw SqlException.typeMismatch("incompatible value relabel");
        if (type.kind() == SqlType.Kind.ARRAY || type.isComposite() || type.kind() == SqlType.Kind.RANGE || type.isInteger()) {
            if (!type.equals(target)) throw SqlException.typeMismatch("structural value relabel requires identical type");
        }
        return new SqlValue(target, isNull, bits, payload);
    }
    public SqlType type() { return type; }
    public boolean isNull() { return isNull; }

    private void require(SqlType.Kind kind) {
        if (isNull) throw new SqlException("22004", "SQL NULL has no payload");
        if (type.kind() != kind) throw SqlException.typeMismatch("expected " + kind + ", got " + type);
    }
    public boolean asBoolean() { require(SqlType.Kind.BOOLEAN); return bits != 0; }
    public long asInteger() {
        if (isNull) throw new SqlException("22004", "SQL NULL has no payload");
        if (!type.isInteger()) throw SqlException.typeMismatch("expected integer, got " + type);
        return bits;
    }
    public String asText() { require(SqlType.Kind.TEXT); return (String) payload; }
    public Decimal asDecimal() { require(SqlType.Kind.DECIMAL); return (Decimal) payload; }
    public double asFloat() {
        if (isNull) throw new SqlException("22004", "SQL NULL has no payload");
        if (!type.isFloat()) throw SqlException.typeMismatch("expected float, got " + type);
        return Double.longBitsToDouble(bits);
    }
    public byte[] asBytes() { require(SqlType.Kind.BYTEA); return ((byte[]) payload).clone(); }

    /** Host conversion boxes only when explicitly crossing the application boundary. */
    public Object toJava() {
        if (isNull) return null;
        return switch (type.kind()) {
            case BOOLEAN -> bits != 0;
            case I16 -> (short) bits;
            case I32 -> (int) bits;
            case I64, TIMESTAMP, TIMESTAMPTZ -> bits;
            case DATE -> (int) bits;
            case F32 -> (float) Double.longBitsToDouble(bits);
            case F64 -> Double.longBitsToDouble(bits);
            case BYTEA -> ((byte[]) payload).clone();
            default -> payload;
        };
    }

    @Override public String toString() {
        if (isNull) return "NULL";
        return switch (type.kind()) {
            case COMPOSITE, RECORD -> CompositeValues.render(this, null);
            case BOOLEAN -> bits != 0 ? "true" : "false";
            case I16, I32, I64 -> Long.toString(bits);
            case DATE -> Temporal.renderDate((int) bits);
            case TIMESTAMP, TIMESTAMPTZ -> Temporal.renderTimestamp(bits, type.kind() == SqlType.Kind.TIMESTAMPTZ);
            case F32 -> SqlFloat.render(Double.longBitsToDouble(bits), true);
            case F64 -> SqlFloat.render(Double.longBitsToDouble(bits), false);
            case BYTEA -> "\\x" + java.util.HexFormat.of().formatHex((byte[]) payload);
            default -> payload.toString();
        };
    }

    @Override public boolean equals(Object other) {
        if (!(other instanceof SqlValue value) || !type.equals(value.type) || isNull != value.isNull) return false;
        if (isNull) return true;
        if (type.isFloat()) {
            double a = asFloat(), b = value.asFloat();
            return a == b || (Double.isNaN(a) && Double.isNaN(b));
        }
        if (type.kind() == SqlType.Kind.BYTEA) return Arrays.equals((byte[]) payload, (byte[]) value.payload);
        return bits == value.bits && Objects.equals(payload, value.payload);
    }

    @Override public int hashCode() {
        if (isNull) return type.hashCode();
        int payloadHash = type.kind() == SqlType.Kind.BYTEA ? Arrays.hashCode((byte[]) payload) : Objects.hashCode(payload);
        long hashBits = bits;
        if (type.isFloat()) {
            double number = asFloat();
            hashBits = Double.doubleToLongBits(number == 0 ? 0 : number);
        }
        return 31 * (31 * type.hashCode() + Long.hashCode(hashBits)) + payloadHash;
    }
}
