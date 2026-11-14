package org.jed;

import java.io.ByteArrayOutputStream;
import java.util.Arrays;
import java.util.List;

/** Canonical keys and single-value bodies. Leaf-region compression is a separate seam. */
final class Encoding {
    private Encoding() {}
    static byte[] terminatedKey(byte[] bytes) { Sink out = new Sink(); terminated(out, bytes); return out.bytes(); }
    static byte[] key(SqlValue value, boolean nullable, boolean descending) {
        byte[] encoded;
        if (nullable) {
            byte[] body = value.isNull() ? new byte[0] : key(value);
            encoded = new byte[body.length + 1]; encoded[0] = (byte) (value.isNull() ? 1 : 0);
            System.arraycopy(body, 0, encoded, 1, body.length);
        } else encoded = key(value);
        if (descending) for (int i = 0; i < encoded.length; i++) encoded[i] = (byte) ~encoded[i];
        return encoded;
    }
    static byte[] key(SqlValue value) {
        if (value.isNull()) throw new SqlException("23502", "NULL storage key");
        Sink out = new Sink();
        switch (value.type().kind()) {
            case JSONB -> JsonCodec.key(out, value.asJsonb(), true);
            case RANGE -> {
                SqlRange range = value.asRange(); out.u8(range.isEmpty() ? 0 : 1);
                if (!range.isEmpty()) {
                    out.u8(range.lower() == null ? 0 : 1); if (range.lower() != null) { out.raw(key(range.lower())); out.u8(range.lowerInclusive() ? 0 : 1); }
                    out.u8(range.upper() == null ? 2 : 1); if (range.upper() != null) { out.raw(key(range.upper())); out.u8(range.upperInclusive() ? 1 : 0); }
                }
            }
            case COMPOSITE, RECORD -> { for (SqlValue field : value.asComposite()) out.raw(key(field, true, false)); }
            case ARRAY -> {
                SqlArray array = value.asArray();
                for (SqlValue element : array.elements()) { out.u8(element.isNull() ? 2 : 1); if (!element.isNull()) out.raw(key(element)); }
                out.u8(0); out.u8(array.dimensions());
                for (int d = 0; d < array.dimensions(); d++) { out.unsigned(array.length(d), 4); out.signed(array.lowerBound(d), 4); }
            }
            case I16, I32, I64 -> out.signed(value.asInteger(), fixedWidth(value.type()));
            case BOOLEAN -> out.u8(value.asBoolean() ? 1 : 0);
            case UUID -> out.raw(value.asUuid().toBytes());
            case DATE -> out.signed(value.asDate(), 4);
            case TIMESTAMP, TIMESTAMPTZ -> out.signed(value.asTimestamp(), 8);
            case INTERVAL -> { out.signed(value.asInterval().spanHigh(), 8); out.unsigned(value.asInterval().spanLow(), 8); }
            case TEXT -> terminated(out, SqlText.encode(value.asText()));
            case BYTEA -> terminated(out, value.asBytes());
            case F32, F64 -> {
                double number = value.asFloat();
                boolean small = value.type().kind() == SqlType.Kind.F32;
                long bits = small ? Integer.toUnsignedLong(Float.floatToIntBits((float) (number == 0 ? 0 : number)))
                    : Double.doubleToLongBits(number == 0 ? 0 : number);
                long sign = small ? 0x80000000L : Long.MIN_VALUE;
                out.unsigned((bits & sign) != 0 ? ~bits : bits ^ sign, small ? 4 : 8);
            }
            case DECIMAL -> {
                Decimal number = value.asDecimal();
                if (number.isZero()) { out.u8(4); break; }
                String digits = number.coefficientDigits();
                int decimalPoint = digits.length() - number.scale();
                int end = digits.length();
                while (digits.charAt(end - 1) == '0') end--;
                digits = digits.substring(0, end);
                if ((decimalPoint & 1) != 0) digits = "0" + digits;
                if ((digits.length() & 1) != 0) digits += "0";
                Sink body = new Sink();
                body.signed(Math.floorDiv(decimalPoint + 1, 2), 4);
                for (int i = 0; i < digits.length(); i += 2) body.u8((digits.charAt(i) - '0') * 10 + digits.charAt(i + 1) - '0' + 1);
                body.u8(0);
                byte[] bytes = body.bytes();
                if (number.negative()) for (int i = 0; i < bytes.length; i++) bytes[i] = (byte) ~bytes[i];
                out.u8(number.negative() ? 3 : 5); out.raw(bytes);
            }
            default -> throw new SqlException("0A000", "key codec unavailable for " + value.type());
        }
        return out.bytes();
    }
    static byte[] row(List<SqlValue> values) {
        Sink out = new Sink();
        for (SqlValue value : values) write(out, value);
        return out.bytes();
    }
    static List<SqlValue> row(byte[] bytes, List<SqlType> types) {
        Reader in = new Reader(bytes);
        var result = new java.util.ArrayList<SqlValue>(types.size());
        for (SqlType type : types) result.add(read(in, type));
        in.end(); return List.copyOf(result);
    }
    static List<SqlValue> row(org.jed.storage.PersistentTree.Entry entry, List<SqlType> types) {
        return StoredRow.of(entry, types).values();
    }
    static org.jed.storage.PersistentTree.Entry entry(byte[] key, List<SqlValue> values,
            org.jed.storage.PersistentTree.Entry previous, boolean[] changed, int pageSize, CostMeter meter) {
        return entry(key, values, previous, changed, pageSize, meter, new int[0]);
    }
    static org.jed.storage.PersistentTree.Entry entry(byte[] key, List<SqlValue> values,
            org.jed.storage.PersistentTree.Entry previous, boolean[] changed, int pageSize, CostMeter meter, int[] primaryKey) {
        return StoredRow.plan(key, values, previous, changed, pageSize, meter, primaryKey);
    }
    static org.jed.storage.PersistentTree.Entry entry(byte[] key, List<SqlValue> values,
            org.jed.storage.PersistentTree.Entry previous, boolean[] changed, org.jed.storage.PageLayout layout, CostMeter meter, int[] primaryKey) {
        return StoredRow.plan(key, values, previous, changed, layout, meter, primaryKey);
    }
    static StoredRow.ReadCost readCost(org.jed.storage.PersistentTree.Entry entry, List<SqlType> types,
                                      boolean[] touched, int pageSize) {
        return StoredRow.of(entry, types).readCost(touched, pageSize);
    }
    static StoredRow.ReadCost readCost(org.jed.storage.PersistentTree.Entry entry, List<SqlType> types,
                                      boolean[] touched, org.jed.storage.PageLayout layout) {
        return StoredRow.of(entry, types).readCost(touched, layout);
    }
    static byte[] value(SqlValue value) { Sink out = new Sink(); write(out, value); return out.bytes(); }
    static void write(Sink out, SqlValue value) {
        out.u8(value.isNull() ? 1 : 0);
        if (!value.isNull()) body(out, value);
    }
    static void body(Sink out, SqlValue value) {
        switch (value.type().kind()) {
            case JSONPATH -> out.sized(SqlText.encode(value.asJsonPath().toString()));
            case JSON -> out.sized(SqlText.encode(value.asJson()));
            case JSONB -> JsonCodec.write(out, value.asJsonb());
            case RANGE -> {
                SqlRange range = value.asRange();
                out.u8(range.isEmpty() ? 1 : (range.lower() == null ? 2 : 0) | (range.upper() == null ? 4 : 0) | (range.lowerInclusive() ? 8 : 0) | (range.upperInclusive() ? 16 : 0));
                if (!range.isEmpty()) { if (range.lower() != null) body(out, range.lower()); if (range.upper() != null) body(out, range.upper()); }
            }
            case COMPOSITE, RECORD -> {
                List<SqlValue> fields = value.asComposite(); byte[] bitmap = new byte[(fields.size() + 7) / 8];
                for (int i = 0; i < fields.size(); i++) if (fields.get(i).isNull()) bitmap[i / 8] |= (byte) (0x80 >>> (i % 8));
                out.raw(bitmap); for (SqlValue field : fields) if (!field.isNull()) body(out, field);
            }
            case ARRAY -> {
                SqlArray array = value.asArray(); boolean hasNull = array.elements().stream().anyMatch(SqlValue::isNull);
                out.u8(array.dimensions()); out.u8(hasNull ? 1 : 0);
                for (int d = 0; d < array.dimensions(); d++) { out.unsigned(array.length(d), 4); out.unsigned(array.lowerBound(d), 4); }
                if (hasNull) {
                    byte[] bitmap = new byte[(array.size() + 7) / 8];
                    for (int i = 0; i < array.size(); i++) if (array.element(i).isNull()) bitmap[i / 8] |= (byte) (0x80 >>> (i % 8));
                    out.raw(bitmap);
                }
                for (SqlValue element : array.elements()) if (!element.isNull()) body(out, element);
            }
            case I16, I32, I64 -> out.signed(value.asInteger(), fixedWidth(value.type()));
            case BOOLEAN -> out.u8(value.asBoolean() ? 1 : 0);
            case UUID -> out.raw(value.asUuid().toBytes());
            case DATE -> out.signed(value.asDate(), 4);
            case TIMESTAMP, TIMESTAMPTZ -> out.signed(value.asTimestamp(), 8);
            case INTERVAL -> { out.unsigned(value.asInterval().months(), 4); out.unsigned(value.asInterval().days(), 4); out.unsigned(value.asInterval().micros(), 8); }
            case TEXT -> out.sized(SqlText.encode(value.asText()));
            case BYTEA -> out.sized(value.asBytes());
            case F32 -> out.unsigned(Float.floatToIntBits((float) value.asFloat()), 4);
            case F64 -> out.unsigned(Double.doubleToLongBits(value.asFloat()), 8);
            case DECIMAL -> {
                Decimal number = value.asDecimal();
                String digits = number.coefficientDigits();
                int count = number.isZero() ? 0 : (digits.length() + 3) / 4;
                out.u8(number.negative() ? 1 : 0); out.u16(number.scale()); out.u16(count);
                for (int i = 0; i < count; i++) {
                    int end = digits.length() - (count - i - 1) * 4;
                    int start = Math.max(0, end - 4);
                    out.u16(Integer.parseInt(digits.substring(start, end)));
                }
            }
            default -> throw new SqlException("0A000", "value codec unavailable for " + value.type());
        }
    }
    static SqlValue read(Reader in, SqlType type) {
        int tag = in.u8();
        if (tag == 1) return SqlValue.nullValue(type);
        if (tag != 0) throw corrupt("invalid single-value presence tag");
        return readBody(in, type);
    }
    static SqlValue readBody(Reader in, SqlType type) {
        try {
            return switch (type.kind()) {
                case I16, I32, I64 -> SqlValue.ofInteger(type, in.signed(fixedWidth(type)));
                case BOOLEAN -> {
                    int value = in.u8(); if (value > 1) throw corrupt("invalid boolean");
                    yield SqlValue.ofBoolean(value != 0);
                }
                case TEXT -> SqlValue.ofText(SqlText.decode(in.sized())).relabel(type);
                case BYTEA -> SqlValue.ofBytes(in.sized());
                case UUID -> SqlValue.ofUuid(SqlUuid.fromBytes(in.raw(16)));
                case DATE -> SqlValue.ofDate((int) in.signed(4));
                case TIMESTAMP, TIMESTAMPTZ -> SqlValue.ofTimestamp(type, in.signed(8));
                case INTERVAL -> SqlValue.ofInterval(type, new SqlInterval((int) in.unsigned(4), (int) in.unsigned(4), in.unsigned(8)));
                case F32 -> SqlValue.ofFloat(type, Float.intBitsToFloat((int) in.unsigned(4)));
                case F64 -> SqlValue.ofFloat(type, Double.longBitsToDouble(in.unsigned(8)));
                case JSONPATH -> SqlValue.ofJsonPath(JsonPath.compile(SqlText.decode(in.sized())));
                case JSON -> SqlValue.ofJson(SqlText.decode(in.sized()));
                case JSONB -> SqlValue.ofJsonb(JsonCodec.read(in, 0));
                case RANGE -> {
                    int flags = in.u8(); if (flags > 31 || (flags & 1) != 0 && flags != 1 || (flags & 10) == 10 || (flags & 20) == 20) throw corrupt("invalid range flags");
                    if (flags == 1) yield SqlValue.ofRange(type, SqlRange.empty());
                    SqlValue lower = (flags & 2) == 0 ? readBody(in, type.element()) : null;
                    SqlValue upper = (flags & 4) == 0 ? readBody(in, type.element()) : null;
                    SqlRange range = SqlRange.create(type.element(), lower, upper, (flags & 8) != 0, (flags & 16) != 0);
                    if (range.isEmpty() || !java.util.Objects.equals(lower, range.lower()) || !java.util.Objects.equals(upper, range.upper()) || range.lowerInclusive() != ((flags & 8) != 0) || range.upperInclusive() != ((flags & 16) != 0)) throw corrupt("noncanonical range body");
                    yield SqlValue.ofRange(type, range);
                }
                case COMPOSITE, RECORD -> {
                    int count = type.fields().size(); byte[] bitmap = in.raw((count + 7) / 8);
                    if (count % 8 != 0 && (bitmap[bitmap.length - 1] & ((1 << (8 - count % 8)) - 1)) != 0) throw corrupt("nonzero record bitmap padding");
                    var fields = new java.util.ArrayList<SqlValue>();
                    for (int i = 0; i < count; i++) { SqlType fieldType = type.fields().get(i).type(); fields.add((bitmap[i / 8] & (0x80 >>> (i % 8))) != 0 ? SqlValue.nullValue(fieldType) : readBody(in, fieldType)); }
                    yield SqlValue.ofComposite(type, fields);
                }
                case ARRAY -> {
                    int dimensions = in.u8(), flags = in.u8();
                    if (flags > 1 || dimensions == 0 && flags != 0) throw corrupt("invalid array header");
                    int[] dims = new int[dimensions], lower = new int[dimensions]; long count = dimensions == 0 ? 0 : 1;
                    for (int d = 0; d < dimensions; d++) {
                        long length = in.unsigned(4); lower[d] = (int) in.unsigned(4);
                        if (length < 1 || length > Integer.MAX_VALUE || count > Integer.MAX_VALUE / length) throw corrupt("invalid array dimensions");
                        dims[d] = (int) length; count *= length;
                    }
                    byte[] bitmap = flags == 1 ? in.raw((int) ((count + 7) / 8)) : null;
                    if (bitmap != null && count % 8 != 0 && (bitmap[bitmap.length - 1] & ((1 << (8 - count % 8)) - 1)) != 0) throw corrupt("nonzero array bitmap padding");
                    if (bitmap == null && count > in.remaining()) throw corrupt("truncated array elements");
                    var elements = new java.util.ArrayList<SqlValue>();
                    for (int i = 0; i < count; i++) elements.add(bitmap != null && (bitmap[i / 8] & (0x80 >>> (i % 8))) != 0 ? SqlValue.nullValue(type.element()) : readBody(in, type.element()));
                    yield SqlValue.ofArray(type, new SqlArray(dims, lower, elements));
                }
                case DECIMAL -> {
                    int flags = in.u8(), scale = in.u16(), count = in.u16();
                    if (flags > 1 || scale > Decimal.MAX_SCALE || count > in.remaining() / 2) throw corrupt("invalid decimal header");
                    if (count == 0) {
                        if (flags != 0) throw corrupt("negative decimal zero");
                        yield SqlValue.ofDecimal(Decimal.zero(scale)).relabel(type);
                    }
                    StringBuilder digits = new StringBuilder(count * 4 + scale + 3);
                    for (int i = 0; i < count; i++) {
                        int group = in.u16();
                        if (group > 9999 || i == 0 && group == 0) throw corrupt("noncanonical decimal group");
                        String part = Integer.toString(group);
                        if (i != 0) digits.append("0".repeat(4 - part.length()));
                        digits.append(part);
                    }
                    if (scale != 0) {
                        if (digits.length() <= scale) digits.insert(0, "0".repeat(scale + 1 - digits.length()));
                        digits.insert(digits.length() - scale, '.');
                    }
                    if (flags != 0) digits.insert(0, '-');
                    yield SqlValue.ofDecimal(Decimal.parse(digits.toString())).relabel(type);
                }
                default -> throw new SqlException("0A000", "value decoder unavailable for " + type);
            };
        } catch (SqlException failure) {
            if (failure.sqlState().equals("0A000") || failure.sqlState().equals("XX001")) throw failure;
            throw corrupt("invalid " + type + " body");
        }
    }
    static int fixedWidth(SqlType type) {
        return switch (type.kind()) {
            case BOOLEAN -> 1;
            case I16 -> 2;
            case I32, F32, DATE -> 4;
            case I64, F64, TIMESTAMP, TIMESTAMPTZ -> 8;
            case UUID, INTERVAL -> 16;
            case DECIMAL -> type.precision() < 0 || type.precision() > 18 ? 0 : type.precision() <= 4 ? 2 : type.precision() <= 9 ? 4 : 8;
            default -> 0;
        };
    }
    private static void terminated(Sink out, byte[] bytes) {
        for (byte value : bytes) { out.u8(value); if (value == 0) out.u8(255); }
        out.u8(0); out.u8(1);
    }
    static SqlException corrupt(String message) { return new SqlException("XX001", message); }
    static final class Sink {
        private final ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        private final boolean checked;
        Sink() { this(true); }
        Sink(boolean checked) { this.checked = checked; }
        int size() { return bytes.size(); }
        void u8(int value) { bytes.write(value); }
        void u16(int value) {
            if (checked && (value < 0 || value > 65535)) throw new SqlException("0A000", "encoded length exceeds u16");
            unsigned(value, 2);
        }
        void unsigned(long value, int width) { for (int i = width - 1; i >= 0; i--) u8((int) (value >>> (i * 8))); }
        void signed(long value, int width) { unsigned(value ^ (1L << (width * 8 - 1)), width); }
        void raw(byte[] value) { bytes.writeBytes(value); }
        void sized(byte[] value) { u16(value.length); raw(value); }
        byte[] bytes() { return bytes.toByteArray(); }
    }
    static final class Reader {
        private final byte[] bytes;
        private int at;
        Reader(byte[] bytes) { this.bytes = bytes; }
        int position() { return at; }
        int remaining() { return bytes.length - at; }
        int u8() { need(1); return bytes[at++] & 255; }
        int u16() { return (int) unsigned(2); }
        long unsigned(int width) { need(width); long value = 0; for (int i = 0; i < width; i++) value = (value << 8) | u8(); return value; }
        long signed(int width) {
            long value = unsigned(width) ^ (1L << (width * 8 - 1));
            return value << (64 - width * 8) >> (64 - width * 8);
        }
        byte[] raw(int size) { need(size); byte[] value = Arrays.copyOfRange(bytes, at, at + size); at += size; return value; }
        byte[] sized() { return raw(u16()); }
        void end() { if (remaining() != 0) throw corrupt("trailing value bytes"); }
        private void need(int size) { if (size < 0 || size > remaining()) throw corrupt("truncated value bytes"); }
    }
}
