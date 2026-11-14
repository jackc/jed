package org.jed;

import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.HexFormat;

/** Shared PRNG bytes and host source-control/sharing contracts, beyond SQL corpus reach. */
public final class EntropySourcesTest {
    public static void main(String[] args) throws Exception {
        String fixtures;
        try (var input = EntropySourcesTest.class.getResourceAsStream("/prng.tsv")) { fixtures = new String(input.readAllBytes(), StandardCharsets.UTF_8); }
        for (String line : fixtures.split("\n")) {
            if (line.startsWith("#") || line.isEmpty()) continue;
            String[] fields = line.split("\t"); long seed = Long.parseUnsignedLong(fields[0]);
            byte[] expected = HexFormat.of().parseHex(fields[1]), actual = new byte[expected.length];
            SeededRandom random = new SeededRandom(seed); random.accept(actual);
            if (!Arrays.equals(actual, expected)) throw new AssertionError("PRNG fixture " + seed);
            random.reseed(seed); byte[] partial = new byte[3]; random.accept(partial);
            if (!Arrays.equals(partial, Arrays.copyOf(expected, 3))) throw new AssertionError("partial fill");
            byte[] next = new byte[8]; random.accept(next);
            if (!Arrays.equals(next, Arrays.copyOfRange(expected, 8, 16))) throw new AssertionError("partial draw count");
        }
        ManualClock clock = new ManualClock(5); clock.advance(-2); if (clock.getAsLong() != 3) throw new AssertionError(); clock.set(7);
        AdvancingClock advancing = new AdvancingClock(10, 3); if (advancing.getAsLong() != 10 || advancing.getAsLong() != 13) throw new AssertionError();
        advancing.reset(2, 5); if (advancing.getAsLong() != 2) throw new AssertionError();
        SeededRandom source = new SeededRandom(1);
        Environment environment = Environment.builder().randomSource(source).clockSource(clock).keyEntropy(() -> new SeededRandom(8)).build();
        Environment derived = environment.toBuilder().build();
        byte[] a = new byte[8], b = new byte[8]; environment.random(a); derived.random(b);
        if (Arrays.equals(a, b) || derived.clock() != 7) throw new AssertionError("environment source sharing");
        environment.newKeyEntropyStream().accept(a); environment.newKeyEntropyStream().accept(b);
        if (!Arrays.equals(a, b)) throw new AssertionError("key stream reset");
        System.out.println("EntropySourcesTest: shared PRNG vectors, source controls and independent key streams passed");
    }
}
