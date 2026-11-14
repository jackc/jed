package org.jed;

import java.util.List;
import java.util.Locale;
import java.util.Objects;

/** Immutable type descriptor. Container types retain their recursively declared types. */
public final class SqlType {
    public enum Kind {
        BOOLEAN, I16, I32, I64, F32, F64, TEXT, DECIMAL, BYTEA, UUID,
        DATE, TIMESTAMP, TIMESTAMPTZ, INTERVAL, JSON, JSONB, JSONPATH,
        ARRAY, COMPOSITE, RANGE, UNKNOWN, RECORD
    }

    public record Field(String name, SqlType type, boolean notNull) {
        public Field(String name, SqlType type) { this(name, type, false); }
        public Field { SqlText.validate(name); Objects.requireNonNull(type, "type"); }
    }

    public static final SqlType UNKNOWN = scalar(Kind.UNKNOWN);
    public static final SqlType BOOLEAN = scalar(Kind.BOOLEAN);
    public static final SqlType I16 = scalar(Kind.I16);
    public static final SqlType I32 = scalar(Kind.I32);
    public static final SqlType I64 = scalar(Kind.I64);
    public static final SqlType F32 = scalar(Kind.F32);
    public static final SqlType F64 = scalar(Kind.F64);
    public static final SqlType TEXT = scalar(Kind.TEXT);
    public static final SqlType DECIMAL = scalar(Kind.DECIMAL);
    public static final SqlType BYTEA = scalar(Kind.BYTEA);
    public static final SqlType UUID = scalar(Kind.UUID);
    public static final SqlType DATE = scalar(Kind.DATE);
    public static final SqlType TIMESTAMP = scalar(Kind.TIMESTAMP);
    public static final SqlType TIMESTAMPTZ = scalar(Kind.TIMESTAMPTZ);
    public static final SqlType INTERVAL = scalar(Kind.INTERVAL);
    public static final SqlType JSON = scalar(Kind.JSON);
    public static final SqlType JSONB = scalar(Kind.JSONB);
    public static final SqlType JSONPATH = scalar(Kind.JSONPATH);

    private final Kind kind;
    private final int precision;
    private final int scale;
    private final int maxLength;
    private final SqlType element;
    private final String name;
    private final List<Field> fields;

    private SqlType(Kind kind, int precision, int scale, int maxLength,
                    SqlType element, String name, List<Field> fields) {
        this.kind = kind;
        this.precision = precision;
        this.scale = scale;
        this.maxLength = maxLength;
        this.element = element;
        this.name = name;
        this.fields = List.copyOf(fields);
    }

    private static SqlType scalar(Kind kind) {
        return new SqlType(kind, -1, -1, -1, null, null, List.of());
    }

    public static SqlType decimal(int precision, int scale) {
        if (precision < 1 || precision > 1000 || scale < 0 || scale > precision) {
            throw new SqlException("22023", "invalid decimal precision or scale");
        }
        return new SqlType(Kind.DECIMAL, precision, scale, -1, null, null, List.of());
    }

    public static SqlType timestamp(boolean zoned, int precision) {
        if (precision < 0) throw new SqlException("42601", "timestamp precision must be non-negative");
        return new SqlType(zoned ? Kind.TIMESTAMPTZ : Kind.TIMESTAMP, Math.min(6, precision), -1, -1, null, null, List.of());
    }
    public static SqlType interval(int qualifier, int precision) {
        if (qualifier < 0 || qualifier > 13 || precision < -1) throw new SqlException("42601", "invalid interval modifier");
        if (qualifier == 0 && precision == -1) return INTERVAL;
        return new SqlType(Kind.INTERVAL, Math.min(6, precision), qualifier, -1, null, null, List.of());
    }
    public int intervalFields() { return kind == Kind.INTERVAL ? Math.max(0, scale) : 0; }

    public static SqlType varchar(int length) {
        if (length < 1 || length > 10_485_760) {
            throw new SqlException("22023", "invalid varchar length");
        }
        return new SqlType(Kind.TEXT, -1, -1, length, null, null, List.of());
    }

    SqlType withoutModifiers() {
        return switch (kind) {
            case TEXT -> TEXT; case DECIMAL -> DECIMAL; case TIMESTAMP -> TIMESTAMP;
            case TIMESTAMPTZ -> TIMESTAMPTZ; case INTERVAL -> INTERVAL;
            case ARRAY -> array(element.withoutModifiers()); default -> this;
        };
    }
    public static SqlType array(SqlType element) {
        if (element.kind() == Kind.ARRAY) return element;
        return new SqlType(Kind.ARRAY, -1, -1, -1,
            Objects.requireNonNull(element, "element"), null, List.of());
    }

    public static SqlType range(SqlType element) {
        Objects.requireNonNull(element, "element");
        if (!(element.kind == Kind.I32 || element.kind == Kind.I64 || element.kind == Kind.DECIMAL || element.kind == Kind.DATE
                || element.kind == Kind.TIMESTAMP || element.kind == Kind.TIMESTAMPTZ)) {
            throw new SqlException("42704", "range type does not exist for element: " + element);
        }
        return new SqlType(Kind.RANGE, -1, -1, -1, element, null, List.of());
    }

    public boolean isComposite() { return kind == Kind.COMPOSITE || kind == Kind.RECORD; }
    public static SqlType record(List<Field> fields) {
        return new SqlType(Kind.RECORD, -1, -1, -1, null, null, fields);
    }

    public static SqlType composite(String name, List<Field> fields) {
        return new SqlType(Kind.COMPOSITE, -1, -1, -1, null, SqlText.validate(name), fields);
    }

    public static SqlType named(String name) {
        for (ScalarCatalog.Fact fact : ScalarCatalog.FACTS) {
            if (!fact.aliases().contains(name)) continue;
            return switch (Kind.valueOf(fact.id().toUpperCase(Locale.ROOT))) {
                case BOOLEAN -> BOOLEAN;
                case I16 -> I16;
                case I32 -> I32;
                case I64 -> I64;
                case F32 -> F32;
                case F64 -> F64;
                case TEXT -> TEXT;
                case DECIMAL -> DECIMAL;
                case BYTEA -> BYTEA;
                case UUID -> UUID;
                case DATE -> DATE;
                case TIMESTAMP -> TIMESTAMP;
                case TIMESTAMPTZ -> TIMESTAMPTZ;
                case INTERVAL -> INTERVAL;
                case JSON -> JSON;
                case JSONB -> JSONB;
                case JSONPATH -> JSONPATH;
                default -> throw new IllegalStateException("scalar catalog contains container");
            };
        }
        for (RangeCatalog.Fact fact : RangeCatalog.FACTS) {
            if (fact.aliases().contains(name)) return range(named(fact.element()));
        }
        throw new SqlException("42704", "type does not exist: " + name);
    }

    public Kind kind() { return kind; }
    public int precision() { return precision; }
    public int scale() { return scale; }
    public int maxLength() { return maxLength; }
    public SqlType element() { return element; }
    public String name() { return name; }
    public List<Field> fields() { return fields; }
    public boolean isInteger() { return kind == Kind.I16 || kind == Kind.I32 || kind == Kind.I64; }
    public boolean isFloat() { return kind == Kind.F32 || kind == Kind.F64; }
    public boolean isNumeric() { return isInteger() || isFloat() || kind == Kind.DECIMAL; }

    private static final ScalarCatalog.Fact I16_BOUNDS = integerBounds("i16");
    private static final ScalarCatalog.Fact I32_BOUNDS = integerBounds("i32");
    private static final ScalarCatalog.Fact I64_BOUNDS = integerBounds("i64");

    private static ScalarCatalog.Fact integerBounds(String name) {
        for (ScalarCatalog.Fact fact : ScalarCatalog.FACTS) if (fact.id().equals(name)) return fact;
        throw new IllegalStateException("integer type missing from scalar catalog: " + name);
    }

    public boolean contains(long value) {
        ScalarCatalog.Fact bounds = switch (kind) {
            case I16 -> I16_BOUNDS;
            case I32 -> I32_BOUNDS;
            case I64 -> I64_BOUNDS;
            default -> null;
        };
        return bounds != null && value >= bounds.min() && value <= bounds.max();
    }

    @Override public String toString() {
        return switch (kind) {
            case DECIMAL -> precision < 0 ? "decimal" : "decimal(" + precision + "," + scale + ")";
            case TIMESTAMP, TIMESTAMPTZ -> kind.name().toLowerCase(Locale.ROOT) + (precision < 0 ? "" : "(" + precision + ")");
            case INTERVAL -> "interval" + (intervalFields() == 0 ? "" : " " + DatetimeUnits.QUALIFIERS.get(intervalFields() - 1).name()) + (precision < 0 ? "" : "(" + precision + ")");
            case TEXT -> maxLength < 0 ? "text" : "varchar(" + maxLength + ")";
            case ARRAY -> element + "[]";
            case RANGE -> switch (element.kind) {
                case I32 -> "i32range";
                case I64 -> "i64range";
                case DECIMAL -> "numrange";
                case DATE -> "daterange";
                case TIMESTAMP -> "tsrange";
                case TIMESTAMPTZ -> "tstzrange";
                default -> throw new IllegalStateException("invalid range type");
            };
            case COMPOSITE -> name;
            default -> kind.name().toLowerCase(Locale.ROOT);
        };
    }

    @Override public boolean equals(Object other) {
        if (this == other) return true;
        if (!(other instanceof SqlType t)) return false;
        return kind == t.kind && precision == t.precision && scale == t.scale
            && maxLength == t.maxLength && Objects.equals(element, t.element)
            && Objects.equals(name, t.name) && fields.equals(t.fields);
    }

    @Override public int hashCode() {
        return Objects.hash(kind, precision, scale, maxLength, element, name, fields);
    }
}
