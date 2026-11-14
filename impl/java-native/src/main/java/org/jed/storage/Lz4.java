package org.jed.storage;

import java.io.ByteArrayOutputStream;
import java.util.Arrays;

/** Handwritten pinned LZ4 block codec from spec/fileformat/lz4.md. */
public final class Lz4 {
    private Lz4() {}
    public static byte[] compress(byte[] source) {
        ByteArrayOutputStream out = new ByteArrayOutputStream(source.length);
        int[] table = new int[4096]; Arrays.fill(table, -1);
        int position = 0, anchor = 0, limit = source.length - 12;
        while (position <= limit) {
            int word = word(source, position);
            int hash = (word * 0x9e3779b1) >>> 20;
            int candidate = table[hash]; table[hash] = position;
            if (candidate >= 0 && position - candidate <= 65535 && word(source, candidate) == word) {
                int match = 4;
                while (position + match < source.length - 5 && source[candidate + match] == source[position + match]) match++;
                int literals = position - anchor;
                out.write(Math.min(literals, 15) << 4 | Math.min(match - 4, 15));
                if (literals >= 15) length(out, literals - 15);
                out.write(source, anchor, literals);
                int offset = position - candidate; out.write(offset); out.write(offset >>> 8);
                if (match >= 19) length(out, match - 19);
                position += match; anchor = position;
            } else position++;
        }
        int literals = source.length - anchor;
        out.write(Math.min(literals, 15) << 4);
        if (literals >= 15) length(out, literals - 15);
        out.write(source, anchor, literals);
        return out.toByteArray();
    }
    /** The caller admits output bytes against its scalar budget before decoding untrusted lengths. */
    public static byte[] decompress(byte[] block, int rawLength, int maximumOutputBytes) throws CorruptStorageException {
        if (rawLength < 0 || rawLength > maximumOutputBytes) throw corrupt("LZ4 output length exceeds admitted bound");
        // Validate all lengths and offsets before allocating the advertised output.
        // A tiny corrupt block must not turn its raw-length field into a large allocation.
        decode(block, rawLength, null);
        byte[] output = new byte[rawLength];
        decode(block, rawLength, output);
        return output;
    }
    private static void decode(byte[] block, int rawLength, byte[] output) throws CorruptStorageException {
        int input = 0, written = 0;
        while (input < block.length) {
            int token = block[input++] & 255;
            long literals = token >>> 4;
            if (literals == 15) {
                int extra;
                do { if (input == block.length) throw corrupt("truncated LZ4 literal length"); extra = block[input++] & 255; literals += extra; }
                while (extra == 255);
            }
            if (literals > block.length - input || literals > rawLength - written) throw corrupt("LZ4 literal run exceeds bound");
            if (output != null) System.arraycopy(block, input, output, written, (int) literals);
            input += (int) literals; written += (int) literals;
            if (input == block.length) {
                if (written != rawLength) throw corrupt("LZ4 output length mismatch");
                return;
            }
            if (block.length - input < 2) throw corrupt("truncated LZ4 offset");
            int offset = (block[input] & 255) | (block[input + 1] & 255) << 8; input += 2;
            if (offset == 0 || offset > written) throw corrupt("invalid LZ4 offset");
            long match = (token & 15) + 4;
            if ((token & 15) == 15) {
                int extra;
                do { if (input == block.length) throw corrupt("truncated LZ4 match length"); extra = block[input++] & 255; match += extra; }
                while (extra == 255);
            }
            if (match > rawLength - written) throw corrupt("LZ4 match exceeds output bound");
            int end = written + (int) match;
            if (output == null) written = end;
            else while (written < end) { output[written] = output[written - offset]; written++; }
        }
        throw corrupt("LZ4 block lacks final literal sequence");
    }
    private static int word(byte[] bytes, int at) {
        return (bytes[at] & 255) | (bytes[at + 1] & 255) << 8 | (bytes[at + 2] & 255) << 16 | (bytes[at + 3] & 255) << 24;
    }
    private static void length(ByteArrayOutputStream out, int remaining) {
        while (remaining >= 255) { out.write(255); remaining -= 255; }
        out.write(remaining);
    }
    private static CorruptStorageException corrupt(String message) { return new CorruptStorageException(message); }
}
