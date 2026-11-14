package org.jed;

import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.attribute.PosixFilePermissions;
import java.util.ArrayList;
import java.util.List;

/** Private, self-describing scratch codec. It is independent of the on-disk database format. */
final class SpillRows {
    private static final SqlType.Kind[] KINDS = SqlType.Kind.values();
    private SpillRows() {}
    static Path create(Path directory) throws IOException {
        return Files.createTempFile(directory, "jed-row-", ".spill", PosixFilePermissions.asFileAttribute(PosixFilePermissions.fromString("rw-------")));
    }
    static void write(DataOutputStream out, List<SqlValue> row) throws IOException {
        out.writeInt(row.size());
        for (SqlValue value : row) {
            SqlType type = value.type();
            writeType(out, type);
            out.writeBoolean(value.isNull());
            if (value.isNull()) continue;
            switch (type.kind()) {
                case JSONPATH -> bytes(out, SqlText.encode(value.asJsonPath().toString()));
                case JSON -> bytes(out, SqlText.encode(value.asJson()));
                case JSONB -> bytes(out, Encoding.value(value));
                case RANGE -> bytes(out, Encoding.value(value));
                case COMPOSITE, RECORD -> write(out, value.asComposite());
                case ARRAY -> {
                    SqlArray array = value.asArray(); out.writeInt(array.dimensions());
                    for (int d = 0; d < array.dimensions(); d++) { out.writeInt(array.length(d)); out.writeInt(array.lowerBound(d)); }
                    write(out, array.elements());
                }
                case BOOLEAN -> out.writeBoolean(value.asBoolean());
                case I16, I32, I64 -> out.writeLong(value.asInteger());
                case F32, F64 -> out.writeLong(Double.doubleToRawLongBits(value.asFloat()));
                case UUID -> { out.writeLong(value.asUuid().high()); out.writeLong(value.asUuid().low()); }
                case INTERVAL -> { out.writeInt(value.asInterval().months()); out.writeInt(value.asInterval().days()); out.writeLong(value.asInterval().micros()); }
                case DATE -> out.writeInt(value.asDate());
                case TIMESTAMP, TIMESTAMPTZ -> out.writeLong(value.asTimestamp());
                case TEXT -> bytes(out, SqlText.encode(value.asText()));
                case BYTEA -> bytes(out, value.asBytes());
                case DECIMAL -> bytes(out, SqlText.encode(value.asDecimal().toString()));
                default -> throw new AssertionError(type);
            }
        }
    }
    static List<SqlValue> read(DataInputStream in, int columns) throws IOException {
        if (in.readInt() != columns) throw new IOException("invalid scratch row width");
        List<SqlValue> row = new ArrayList<>(columns);
        for (int i = 0; i < columns; i++) {
            SqlType type = readType(in);
            if (in.readBoolean()) { row.add(SqlValue.nullValue(type)); continue; }
            row.add(switch (type.kind()) {
                case JSONPATH -> SqlValue.ofJsonPath(JsonPath.compile(SqlText.decode(bytes(in))));
                case JSON -> SqlValue.ofJson(SqlText.decode(bytes(in)));
                case JSONB, RANGE -> { Encoding.Reader reader = new Encoding.Reader(bytes(in)); SqlValue value = Encoding.read(reader, type); reader.end(); yield value; }
                case COMPOSITE, RECORD -> SqlValue.ofComposite(type, read(in, type.fields().size()));
                case ARRAY -> {
                    int dimensions = in.readInt(); if (dimensions < 0 || dimensions > 255) throw new IOException("invalid scratch array dimensions");
                    int[] dims = new int[dimensions], lower = new int[dimensions]; long count = dimensions == 0 ? 0 : 1;
                    for (int d = 0; d < dimensions; d++) { dims[d] = in.readInt(); lower[d] = in.readInt(); if (dims[d] <= 0 || count > Integer.MAX_VALUE / dims[d]) throw new IOException("invalid scratch array extent"); count *= dims[d]; }
                    yield SqlValue.ofArray(type, new SqlArray(dims, lower, read(in, (int) count)));
                }
                case BOOLEAN -> SqlValue.ofBoolean(in.readBoolean());
                case I16, I32, I64 -> SqlValue.ofInteger(type, in.readLong());
                case F32, F64 -> SqlValue.ofFloat(type, Double.longBitsToDouble(in.readLong()));
                case UUID -> SqlValue.ofUuid(new SqlUuid(in.readLong(), in.readLong()));
                case INTERVAL -> SqlValue.ofInterval(new SqlInterval(in.readInt(), in.readInt(), in.readLong())).relabel(type);
                case DATE -> SqlValue.ofDate(in.readInt());
                case TIMESTAMP, TIMESTAMPTZ -> SqlValue.ofTimestamp(type.kind() == SqlType.Kind.TIMESTAMP ? SqlType.TIMESTAMP : SqlType.TIMESTAMPTZ, in.readLong()).relabel(type);
                case TEXT -> SqlValue.ofText(SqlText.decode(bytes(in))).relabel(type);
                case BYTEA -> SqlValue.ofBytes(bytes(in));
                case DECIMAL -> SqlValue.ofDecimal(Decimal.parse(SqlText.decode(bytes(in)))).relabel(type);
                default -> throw new AssertionError(type);
            });
        }
        return List.copyOf(row);
    }
    private static void writeType(DataOutputStream out, SqlType type) throws IOException {
            out.writeByte(type.kind().ordinal());
            switch (type.kind()) {
                case DECIMAL -> { out.writeInt(type.precision()); out.writeInt(type.scale()); }
                case TEXT -> out.writeInt(type.maxLength());
                case TIMESTAMP, TIMESTAMPTZ -> out.writeInt(type.precision());
                case INTERVAL -> { out.writeInt(type.intervalFields()); out.writeInt(type.precision()); }
                case ARRAY, RANGE -> writeType(out, type.element());
                case COMPOSITE, RECORD -> {
                    if (type.kind() == SqlType.Kind.COMPOSITE) bytes(out, SqlText.encode(type.name()));
                    out.writeInt(type.fields().size());
                    for (SqlType.Field field : type.fields()) { bytes(out, SqlText.encode(field.name())); writeType(out, field.type()); out.writeBoolean(field.notNull()); }
                }
                case BOOLEAN, I16, I32, I64, F32, F64, BYTEA, UUID, DATE, UNKNOWN, JSON, JSONB, JSONPATH -> { }
                default -> throw new SqlException("0A000", "scratch codec unavailable for " + type);
            }
    }
    private static SqlType readType(DataInputStream in) throws IOException {
            int tag = in.readUnsignedByte();
            if (tag >= KINDS.length) throw new IOException("invalid scratch type");
            return switch (KINDS[tag]) {
                case JSONPATH -> SqlType.JSONPATH; case JSON -> SqlType.JSON; case JSONB -> SqlType.JSONB;
                case UNKNOWN -> SqlType.UNKNOWN; case BOOLEAN -> SqlType.BOOLEAN; case I16 -> SqlType.I16; case I32 -> SqlType.I32; case I64 -> SqlType.I64;
                case F32 -> SqlType.F32; case F64 -> SqlType.F64; case BYTEA -> SqlType.BYTEA;
                case ARRAY -> SqlType.array(readType(in));
                case RANGE -> SqlType.range(readType(in));
                case COMPOSITE, RECORD -> {
                    String name = KINDS[tag] == SqlType.Kind.COMPOSITE ? SqlText.decode(bytes(in)) : null;
                    int count = in.readInt(); if (count < 0 || count > 65535) throw new IOException("invalid scratch record arity");
                    var fields = new ArrayList<SqlType.Field>();
                    for (int i = 0; i < count; i++) fields.add(new SqlType.Field(SqlText.decode(bytes(in)), readType(in), in.readBoolean()));
                    yield name == null ? SqlType.record(fields) : SqlType.composite(name, fields);
                }
                case INTERVAL -> SqlType.interval(in.readInt(), in.readInt());
                case UUID -> SqlType.UUID; case DATE -> SqlType.DATE;
                case TIMESTAMP, TIMESTAMPTZ -> { int precision = in.readInt(); boolean zoned = KINDS[tag] == SqlType.Kind.TIMESTAMPTZ; yield precision < 0 ? (zoned ? SqlType.TIMESTAMPTZ : SqlType.TIMESTAMP) : SqlType.timestamp(zoned, precision); }
                case TEXT -> { int length = in.readInt(); yield length < 0 ? SqlType.TEXT : SqlType.varchar(length); }
                case DECIMAL -> { int precision = in.readInt(), scale = in.readInt(); yield precision < 0 ? SqlType.DECIMAL : SqlType.decimal(precision, scale); }
                default -> throw new IOException("unsupported scratch type");
            };
    }
    private static void bytes(DataOutputStream out, byte[] value) throws IOException { out.writeInt(value.length); out.write(value); }
    private static byte[] bytes(DataInputStream in) throws IOException {
        int size = in.readInt();
        if (size < 0) throw new IOException("invalid scratch payload length");
        byte[] bytes = in.readNBytes(size);
        if (bytes.length != size) throw new EOFException("truncated scratch payload");
        return bytes;
    }
    static SqlException io(IOException failure) { return new SqlException("58030", "scratch I/O failed: " + failure.getMessage()); }
}
