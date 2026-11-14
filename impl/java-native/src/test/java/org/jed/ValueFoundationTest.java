package org.jed;

import java.math.BigDecimal;
import java.math.BigInteger;
import java.math.RoundingMode;
import java.util.Random;

/** Internal limb oracle and host-boundary checks, which SQL corpus files cannot express. */
public final class ValueFoundationTest {
    private static void equal(Object actual, Object expected) {
        if (!actual.equals(expected)) throw new AssertionError(actual + " != " + expected);
    }

    private static void state(String state, Runnable operation) {
        try { operation.run(); }
        catch (SqlException e) { equal(e.sqlState(), state); return; }
        throw new AssertionError("expected " + state);
    }

    private static void temporalFixtures() throws java.io.IOException {
        try (var resource = ValueFoundationTest.class.getResourceAsStream("/temporal.tsv")) {
            if (resource == null) throw new AssertionError("missing temporal fixture resource");
            String text = new String(resource.readAllBytes(), java.nio.charset.StandardCharsets.UTF_8);
            int count = 0;
            for (String row : text.lines().toList()) {
                if (row.startsWith("#") || row.isBlank()) continue;
                String[] fields = row.split("\t", -1);
                String input = new String(java.util.HexFormat.of().parseHex(fields[2]), java.nio.charset.StandardCharsets.UTF_8);
                boolean date = fields[1].equals("date"), zoned = fields[1].equals("timestamptz");
                if (fields[0].equals("parse_error")) state(fields[5], () -> { if (date) Temporal.parseDate(input); else Temporal.parseTimestamp(input, zoned); });
                else if (fields[0].equals("parse")) equal(date ? (long) Temporal.parseDate(input) : Temporal.parseTimestamp(input, zoned), Long.parseLong(fields[3]));
                else {
                    String expected = new String(java.util.HexFormat.of().parseHex(fields[4]), java.nio.charset.StandardCharsets.UTF_8);
                    equal(date ? Temporal.renderDate(Integer.parseInt(fields[3])) : Temporal.renderTimestamp(Long.parseLong(fields[3]), zoned), expected);
                }
                count++;
            }
            System.out.println("Temporal fixtures: " + count + " shared parse/render/error vectors passed");
        }
    }

    private static void intervalFixtures() throws java.io.IOException {
        try (var resource = ValueFoundationTest.class.getResourceAsStream("/interval.tsv")) {
            if (resource == null) throw new AssertionError("missing interval fixture resource");
            String text = new String(resource.readAllBytes(), java.nio.charset.StandardCharsets.UTF_8);
            int count = 0;
            var failures = new java.util.ArrayList<String>();
            for (String row : text.lines().toList()) {
                if (row.startsWith("#") || row.isBlank()) continue;
                String[] fields = row.split("\t", -1);
                String input = new String(java.util.HexFormat.of().parseHex(fields[1]), java.nio.charset.StandardCharsets.UTF_8);
                try {
                    if (fields[0].equals("parse_error")) state(fields[6], () -> SqlInterval.parse(input));
                    else if (fields[0].equals("parse")) {
                        SqlInterval value = SqlInterval.parse(input);
                        equal(value.months(), Integer.parseInt(fields[2])); equal(value.days(), Integer.parseInt(fields[3])); equal(value.micros(), Long.parseLong(fields[4]));
                    } else equal(new SqlInterval(Integer.parseInt(fields[2]), Integer.parseInt(fields[3]), Long.parseLong(fields[4])).toString(), new String(java.util.HexFormat.of().parseHex(fields[5]), java.nio.charset.StandardCharsets.UTF_8));
                } catch (RuntimeException | AssertionError failure) { failures.add(fields[0] + " [" + input + "]: " + failure); }
                count++;
            }
            if (!failures.isEmpty()) throw new AssertionError(String.join("\n", failures));
            System.out.println("Interval fixtures: " + count + " shared parse/render/error vectors passed");
        }
    }

    private static void discardedMeter() {
        try (Database database = Database.memory(); Session session = database.session()) {
            session.setMaxCost(10); session.setLifetimeMaxCost(20); session.setMaxScalarBytes(8);
            Cancellation cancellation = new Cancellation(); CostMeter meter = new CostMeter(session, cancellation);
            meter.charge(2); meter.scalarReserve(3); meter.memory().reserve(32);
            CostMeter.Checkpoint saved = meter.checkpoint();
            meter.charge(12); meter.scalarReserve(5); meter.memory().reserve(64);
            state("54P01", meter::guard); meter.memory().release(64); meter.restore(saved);
            equal(meter.cost(), 2L); equal(session.lifetimeCost(), 2L); equal(meter.memory().used(), 32L);
            meter.guard(); meter.scalarReserve(5); state("54P04", () -> meter.scalarReserve(1));
            cancellation.cancel(); meter.restore(saved); state("57014", meter::guard);
            meter.memory().release(32);
        }
    }
    private static void retainedShapes() throws java.io.IOException {
        equal(JsonCodec.textMemory("{\"x\":[1,2.3,\"é\"]}"), 171L);
        equal(JsonCodec.textMemory("1e5"), 40L);
        equal(JsonCodec.textMemory("1e-5"), 36L);
        equal(JsonCodec.textMemory("0e999999999999999999999999999"), 32L);
        equal(JsonCodec.textMemory("\"\\u0000\""), 33L);
        equal(JsonCodec.textMemory("1e1000000"), 1_000_036L);
        try (Database database = Database.memory(); Session session = database.session()) {
            var input = new ScalarPlan.Expression(SqlType.TEXT, ignored -> SqlValue.nullValue(SqlType.TEXT));
            var fold = new AggregatePlan.Fold("jsonb_agg_strict", input, null, SqlType.JSONB, false, java.util.List.of(), java.util.List.of());
            var state = new AggregatePlan.State(fold);
            state.add(SqlValue.nullValue(SqlType.TEXT), new CostMeter(session, new Cancellation()));
            equal(state.count, 0L); equal(state.retainedBytes(), 0L);
            var bytes = new java.io.ByteArrayOutputStream(); state.write(new java.io.DataOutputStream(bytes));
            var restored = new AggregatePlan.State(fold);
            restored.read(new java.io.DataInputStream(new java.io.ByteArrayInputStream(bytes.toByteArray())));
            equal(restored.count, 0L); equal(restored.finish().asJsonb().render(), "[]");
        }
        java.nio.file.Path scratch = java.nio.file.Files.createTempDirectory("jed-retained-shapes-");
        MemoryAccount memory = new MemoryAccount(0);
        try {
            try (AggregateCollections values = new AggregateCollections(scratch, 4096, memory)) {
                values.put(0, 0, 0, java.util.List.of(SqlValue.ofText("a")), false);
                equal(memory.used(), 97L); // hash entry + row + text value
                if (!values.unique(0, 0, "a")) throw new AssertionError("first unique key missing");
                equal(memory.used(), 226L); // ordinal, aggregate index, key
                values.discard(0, 0, 1, true);
                equal(memory.used(), 0L);
                values.unique(1, 0, "skipped");
                values.discard(1, 0, 0, true); // strict object inputs can retain only a key
                equal(memory.used(), 0L);
            }
            equal(memory.used(), 0L);
        } finally { java.nio.file.Files.delete(scratch); }
    }
    public static void main(String[] args) throws java.io.IOException {
        discardedMeter();
        retainedShapes();
        temporalFixtures();
        intervalFixtures();
        Random random = new Random(0x4a45444cL);
        for (int i = 0; i < 2000; i++) {
            BigInteger a = new BigInteger(1 + random.nextInt(500), random);
            BigInteger b = new BigInteger(1 + random.nextInt(400), random).add(BigInteger.ONE);
            if (random.nextBoolean()) a = a.negate();
            if (random.nextBoolean()) b = b.negate();
            BigDecimal x = new BigDecimal(a, random.nextInt(70));
            BigDecimal y = new BigDecimal(b, random.nextInt(70));
            Decimal left = Decimal.parse(x.toPlainString()), right = Decimal.parse(y.toPlainString());
            equal(left.add(right).toString(), x.add(y).toPlainString());
            equal(left.subtract(right).toString(), x.subtract(y).toPlainString());
            equal(left.multiply(right).toString(), x.multiply(y).toPlainString());
            int scale = random.nextInt(80);
            equal(left.divide(right, scale).toString(), x.divide(y, scale, RoundingMode.HALF_UP).toPlainString());
            equal(left.rescale(scale).toString(), x.setScale(scale, RoundingMode.HALF_UP).toPlainString());
            equal(left.truncate(scale).toString(), x.setScale(scale, RoundingMode.DOWN).toPlainString());
            equal(Integer.signum(left.compareTo(right)), Integer.signum(x.compareTo(y)));
            equal(left.modulo(right).toString(), x.remainder(y).setScale(Math.max(x.scale(), y.scale())).toPlainString());
            Decimal padded = left.rescale(left.scale() + 3);
            equal(left, padded);
            equal(left.hashCode(), padded.hashCode());
            int radix = switch (i % 3) { case 0 -> 16; case 1 -> 8; default -> 2; };
            String prefix = radix == 16 ? "0x" : radix == 8 ? "0o" : "0b";
            equal(Decimal.parse((a.signum() < 0 ? "-" : "") + prefix + a.abs().toString(radix)).toString(), a.toString());
            if (i < 100) equal(Decimal.parse(a.multiply(a).toString()).squareRootAtScale(0).toString(), a.abs().toString());
        }
        for (long value : new long[]{Long.MIN_VALUE, Long.MAX_VALUE, 0, 1, -1, 1_000_000_000L}) {
            equal(Decimal.of(value).toLongExact(), value);
        }
        for (int i = 0; i < 1000; i++) {
            double number = Double.longBitsToDouble(random.nextLong());
            if (!Double.isFinite(number)) continue;
            equal(Decimal.fromDoubleExact(number).toString(), new BigDecimal(number).toPlainString());
            String rendered = SqlValue.ofFloat(SqlType.F64, number).toString();
            equal(Double.doubleToLongBits(Double.parseDouble(rendered)), Double.doubleToLongBits(number));
            float small = Float.intBitsToFloat(random.nextInt());
            if (Float.isFinite(small)) {
                String narrow = SqlValue.ofFloat(SqlType.F32, small).toString();
                equal(Float.floatToIntBits(Float.parseFloat(narrow)), Float.floatToIntBits(small));
            }
        }
        state("22003", () -> Decimal.parse("9223372036854775808").toLongExact());
        state("22003", () -> Decimal.parse("-9223372036854775809").toLongExact());
        byte[] bytes = {1, 2, 3};
        SqlValue value = SqlValue.ofBytes(bytes);
        bytes[0] = 9;
        byte[] copy = value.asBytes();
        copy[0] = 7;
        equal(value.asBytes()[0], (byte) 1);
        state("22021", () -> SqlText.encode("\ud800"));
        state("22021", () -> SqlText.encode("\udc00"));
        state("22021", () -> SqlText.decode(new byte[]{(byte) 0xc0, (byte) 0x80}));
        state("22021", () -> SqlText.decode(new byte[]{0}));
        equal(SqlText.decode(SqlText.encode("A😀\ue000")), "A😀\ue000");
        equal(SqlText.compare("😀", "\ue000"), 1);
        equal(SqlText.length("A😀"), 2);
        equal(SqlValue.ofInteger(SqlType.I16, 4).toJava().getClass(), Short.class);
        equal(SqlValue.ofInteger(SqlType.I32, 4).toJava().getClass(), Integer.class);
        equal(Double.doubleToRawLongBits(SqlValue.ofFloat(SqlType.F64, -0.0).asFloat()), Long.MIN_VALUE);
        long nanBits = 0x7ff8000000000123L;
        equal(Double.doubleToRawLongBits(SqlValue.ofFloat(SqlType.F64, Double.longBitsToDouble(nanBits)).asFloat()), nanBits);
        equal(SqlValue.ofFloat(SqlType.F64, -0.0), SqlValue.ofFloat(SqlType.F64, 0.0));
        equal(SqlValue.ofFloat(SqlType.F64, -0.0).hashCode(), SqlValue.ofFloat(SqlType.F64, 0.0).hashCode());
        state("22004", () -> SqlValue.nullValue(SqlType.I64).asInteger());
        SqlUuid uuid = new SqlUuid(0x8123456789abcdefL, 0xfedcba9876543210L);
        equal(SqlCasts.cast(SqlCasts.cast(SqlValue.ofUuid(uuid), SqlType.BYTEA), SqlType.UUID), SqlValue.ofUuid(uuid));
        state("22P02", () -> SqlCasts.cast(SqlValue.ofBytes(new byte[15]), SqlType.UUID));
        state("22P02", () -> SqlCasts.cast(SqlValue.ofBytes(new byte[17]), SqlType.UUID));
        for (int i = 0; i < 1000; i++) {
            int day = random.nextInt();
            if (day == Integer.MIN_VALUE || day == Integer.MAX_VALUE) continue;
            Temporal.Civil date = Temporal.civil(day);
            equal(Temporal.days(date.year(), date.month(), date.day()), (long) day);
            long micros = random.nextLong();
            if (micros == Long.MIN_VALUE || micros == Long.MAX_VALUE) continue;
            equal(Temporal.assemble(Math.floorDiv(micros, Temporal.DAY_MICROS), Math.floorMod(micros, Temporal.DAY_MICROS)), micros);
        }
        System.out.println("ValueFoundationTest: 2000 limb oracle cases, 1000 float cases and host invariants passed");
    }
}
