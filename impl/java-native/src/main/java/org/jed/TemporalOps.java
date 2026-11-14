package org.jed;

/** Checked calendar arithmetic; each completed calendar step must remain finite. */
final class TemporalOps {
    private TemporalOps() {}
    static ScalarPlan.Expression resolve(String op, ScalarPlan.Expression left, ScalarPlan.Expression right) {
        SqlType.Kind a = left.type().kind(), b = right.type().kind();
        SqlType result = null;
        if ((op.equals("*") || op.equals("/")) && a == SqlType.Kind.INTERVAL && (right.type().isInteger() || b == SqlType.Kind.DECIMAL)
            || op.equals("*") && b == SqlType.Kind.INTERVAL && (left.type().isInteger() || a == SqlType.Kind.DECIMAL)) {
            return new ScalarPlan.Expression(SqlType.INTERVAL, meter -> {
                meter.operation(); SqlValue x = left.run(meter);
                if (x.isNull()) return SqlValue.nullValue(SqlType.INTERVAL);
                SqlValue y = right.run(meter);
                if (y.isNull()) return SqlValue.nullValue(SqlType.INTERVAL);
                SqlValue number = a == SqlType.Kind.INTERVAL ? y : x;
                Decimal factor = number.type().isInteger() ? Decimal.of(number.asInteger()) : number.asDecimal();
                return SqlValue.ofInterval(IntervalScale.apply((a == SqlType.Kind.INTERVAL ? x : y).asInterval(), factor, op.equals("/")));
            });
        }
        if ((op.equals("+") || op.equals("-")) && a == SqlType.Kind.INTERVAL && b == a) result = SqlType.INTERVAL;
        else if ((op.equals("+") || op.equals("-")) && a == SqlType.Kind.DATE && right.type().isInteger()) result = SqlType.DATE;
        else if (op.equals("+") && left.type().isInteger() && b == SqlType.Kind.DATE) result = SqlType.DATE;
        else if (op.equals("-") && a == SqlType.Kind.DATE && b == a) result = SqlType.I32;
        else if ((op.equals("+") || op.equals("-")) && temporal(a) && b == SqlType.Kind.INTERVAL) result = timestampType(a);
        else if (op.equals("+") && a == SqlType.Kind.INTERVAL && temporal(b)) result = timestampType(b);
        else if (op.equals("-") && timestamp(a) && b == a) result = SqlType.INTERVAL;
        if (result == null) return null;
        SqlType target = result;
        return new ScalarPlan.Expression(target, meter -> {
            meter.operation();
            SqlValue x = left.run(meter);
            if (x.isNull()) return SqlValue.nullValue(target);
            SqlValue y = right.run(meter);
            if (y.isNull()) return SqlValue.nullValue(target);
            boolean subtract = op.equals("-");
            if (a == SqlType.Kind.INTERVAL && b == a)
                return SqlValue.ofInterval(subtract ? x.asInterval().subtract(y.asInterval()) : x.asInterval().add(y.asInterval()));
            if (a == SqlType.Kind.DATE && right.type().isInteger()) return SqlValue.ofDate(dateShift(x.asDate(), y.asInteger(), subtract));
            if (left.type().isInteger() && b == SqlType.Kind.DATE) return SqlValue.ofDate(dateShift(y.asDate(), x.asInteger(), false));
            if (a == SqlType.Kind.DATE && b == a) {
                if (infiniteDate(x.asDate()) || infiniteDate(y.asDate())) throw Temporal.overflow();
                long days = (long) x.asDate() - y.asDate();
                if (days < Integer.MIN_VALUE || days > Integer.MAX_VALUE) throw Temporal.overflow();
                return SqlValue.ofInteger(SqlType.I32, days);
            }
            if (timestamp(a) && b == a) {
                long first = x.asTimestamp(), second = y.asTimestamp();
                if (infinite(first) || infinite(second)) throw Temporal.overflow();
                try {
                    long difference = Math.subtractExact(first, second);
                    return SqlValue.ofInterval(new SqlInterval(0, (int) (difference / Temporal.DAY_MICROS), difference % Temporal.DAY_MICROS));
                } catch (ArithmeticException failure) { throw Temporal.overflow(); }
            }
            SqlValue base = a == SqlType.Kind.INTERVAL ? y : x;
            SqlInterval delta = (a == SqlType.Kind.INTERVAL ? x : y).asInterval();
            long instant = base.type().kind() == SqlType.Kind.DATE ? dateTimestamp(base.asDate()) : base.asTimestamp();
            return SqlValue.ofTimestamp(target, shift(instant, delta, subtract, target.kind() == SqlType.Kind.TIMESTAMPTZ, meter));
        });
    }
    static ScalarPlan.Expression negateInterval(ScalarPlan.Expression operand) {
        return new ScalarPlan.Expression(SqlType.INTERVAL, meter -> {
            meter.operation(); SqlValue value = operand.run(meter);
            return value.isNull() ? SqlValue.nullValue(SqlType.INTERVAL) : SqlValue.ofInterval(value.asInterval().negate());
        });
    }
    private static boolean temporal(SqlType.Kind kind) { return kind == SqlType.Kind.DATE || timestamp(kind); }
    private static boolean timestamp(SqlType.Kind kind) { return kind == SqlType.Kind.TIMESTAMP || kind == SqlType.Kind.TIMESTAMPTZ; }
    private static SqlType timestampType(SqlType.Kind kind) { return kind == SqlType.Kind.TIMESTAMPTZ ? SqlType.TIMESTAMPTZ : SqlType.TIMESTAMP; }
    private static boolean infiniteDate(int days) { return days == Integer.MIN_VALUE || days == Integer.MAX_VALUE; }
    static boolean infinite(long micros) { return micros == Long.MIN_VALUE || micros == Long.MAX_VALUE; }
    static long dateTimestamp(int day) {
        return day == Integer.MIN_VALUE ? Long.MIN_VALUE : day == Integer.MAX_VALUE ? Long.MAX_VALUE : Temporal.assemble(day, 0);
    }
    private static int dateShift(int day, long delta, boolean subtract) {
        if (infiniteDate(day)) return day;
        try {
            long result = subtract ? Math.subtractExact(day, delta) : Math.addExact(day, delta);
            if (result <= Integer.MIN_VALUE || result >= Integer.MAX_VALUE) throw Temporal.overflow();
            return (int) result;
        } catch (ArithmeticException failure) { throw Temporal.overflow(); }
    }
    static long shift(long instant, SqlInterval delta, boolean subtract, boolean zoned, CostMeter meter) {
        if (infinite(instant)) return instant;
        long direction = subtract ? -1 : 1;
        if (delta.months() != 0) {
            if (zoned) { meter.charge(Costs.TIMEZONE); meter.guard(); }
            if (zoned) instant = meter.zone().toLocal(instant);
            long day = Math.floorDiv(instant, Temporal.DAY_MICROS), time = Math.floorMod(instant, Temporal.DAY_MICROS);
            Temporal.Civil civil = Temporal.civil(day);
            long month = civil.year() * 12 + civil.month() - 1 + direction * delta.months();
            long year = Math.floorDiv(month, 12); int number = Math.floorMod(month, 12) + 1;
            instant = Temporal.assemble(Temporal.days(year, number, Math.min(civil.day(), Temporal.monthDays(year, number))), time);
            if (zoned) instant = meter.zone().toInstant(instant);
        }
        if (delta.days() != 0) {
            if (zoned) { meter.charge(Costs.TIMEZONE); meter.guard(); }
            if (zoned) instant = meter.zone().toLocal(instant);
            instant = Temporal.assemble(Math.floorDiv(instant, Temporal.DAY_MICROS) + direction * delta.days(), Math.floorMod(instant, Temporal.DAY_MICROS));
            if (zoned) instant = meter.zone().toInstant(instant);
        }
        try {
            instant = subtract ? Math.subtractExact(instant, delta.micros()) : Math.addExact(instant, delta.micros());
            if (infinite(instant)) throw Temporal.overflow();
            return instant;
        } catch (ArithmeticException failure) { throw Temporal.overflow(); }
    }
}
