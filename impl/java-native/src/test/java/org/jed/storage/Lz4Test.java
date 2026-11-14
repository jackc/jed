package org.jed.storage;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.HexFormat;
import java.util.Random;
import java.util.regex.Pattern;

/** Shared byte vectors and malformed bounded decoder inputs. */
public final class Lz4Test {
    public static void main(String[] args) throws Exception {
        Path path = Path.of(args.length == 0 ? "spec/fileformat/lz4_vectors.toml" : args[0]);
        String source = Files.readString(path);
        var vectors = Pattern.compile("name = \"([^\"]+)\"\\s+input_hex = \"([0-9a-f]*)\"\\s+compressed_hex = \"([0-9a-f]*)\"").matcher(source);
        int count = 0;
        while (vectors.find()) {
            byte[] input = HexFormat.of().parseHex(vectors.group(2)), expected = HexFormat.of().parseHex(vectors.group(3));
            check(Arrays.equals(Lz4.compress(input), expected), "encoder differs: " + vectors.group(1));
            check(Arrays.equals(Lz4.decompress(expected, input.length, input.length), input), "decoder differs: " + vectors.group(1)); count++;
        }
        check(count > 10, "shared vector inventory");
        Random random = new Random(281);
        for (int n = 0; n < 1000; n++) {
            byte[] input = new byte[random.nextInt(5000)]; random.nextBytes(input);
            if ((n & 1) == 0) for (int i = 15; i < input.length; i++) input[i] = input[i % 15];
            check(Arrays.equals(Lz4.decompress(Lz4.compress(input), input.length, input.length), input), "random round trip");
        }
        for (String malformed : new String[]{"", "f0", "ff00", "1000ff", "10000000", "10000200", "1f000100ff"}) {
            try { Lz4.decompress(HexFormat.of().parseHex(malformed), 100, 100); throw new AssertionError("accepted malformed LZ4 " + malformed); }
            catch (CorruptStorageException expected) { }
        }
        try { Lz4.decompress(new byte[]{0}, 100, 99); throw new AssertionError("output admission ignored"); }
        catch (CorruptStorageException expected) { }
        System.out.println("Lz4Test: passed " + count + " shared vectors and bounded malformed decoder checks");
    }
    private static void check(boolean condition, String message) { if (!condition) throw new AssertionError(message); }
}
