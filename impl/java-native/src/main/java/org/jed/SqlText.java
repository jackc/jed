package org.jed;

import java.nio.charset.StandardCharsets;
import java.util.Objects;

/** UTF-8 SQL text. Host UTF-16 strings must not silently replace lone surrogates. */
public final class SqlText {
    private SqlText() {}

    public static String validate(String text) {
        Objects.requireNonNull(text, "text");
        for (int i = 0; i < text.length(); i++) {
            char c = text.charAt(i);
            if (c == 0) throw badBytes(new byte[]{0}, 0, 1);
            if (Character.isHighSurrogate(c)) {
                if (++i == text.length() || !Character.isLowSurrogate(text.charAt(i))) {
                    throw new SqlException("22021", "invalid Unicode surrogate in text");
                }
            } else if (Character.isLowSurrogate(c)) {
                throw new SqlException("22021", "invalid Unicode surrogate in text");
            }
        }
        return text;
    }

    public static byte[] encode(String text) {
        return validate(text).getBytes(StandardCharsets.UTF_8);
    }

    public static String decode(byte[] bytes) {
        for (int i = 0; i < bytes.length;) {
            int lead = bytes[i] & 255;
            int width = lead < 0x80 ? 1 : (lead & 0xe0) == 0xc0 ? 2 : (lead & 0xf0) == 0xe0 ? 3 : (lead & 0xf8) == 0xf0 ? 4 : 1;
            boolean valid = lead > 0 && (lead < 0x80 || (lead >= 0xc2 && lead <= 0xf4));
            if (i + width > bytes.length) valid = false;
            for (int j = 1; j < width && i + j < bytes.length; j++) {
                if ((bytes[i + j] & 0xc0) != 0x80) valid = false;
            }
            if (width >= 3 && i + 1 < bytes.length) {
                int second = bytes[i + 1] & 255;
                if (lead == 0xe0 && second < 0xa0 || lead == 0xed && second >= 0xa0
                        || lead == 0xf0 && second < 0x90 || lead == 0xf4 && second >= 0x90) valid = false;
            }
            if (!valid) throw badBytes(bytes, i, width);
            i += width;
        }
        return new String(bytes, StandardCharsets.UTF_8);
    }

    private static SqlException badBytes(byte[] bytes, int start, int width) {
        StringBuilder message = new StringBuilder("invalid byte sequence for encoding \"UTF8\":");
        for (int i = start; i < Math.min(bytes.length, start + width); i++) {
            int b = bytes[i] & 255;
            message.append(" 0x").append(Character.forDigit(b >>> 4, 16)).append(Character.forDigit(b & 15, 16));
        }
        return new SqlException("22021", message.toString());
    }

    /** C collation orders Unicode scalar values, unlike String.compareTo's UTF-16 order. */
    public static int compare(String left, String right) {
        int a = 0, b = 0;
        while (a < left.length() && b < right.length()) {
            int x = left.codePointAt(a), y = right.codePointAt(b);
            if (x != y) return Integer.compare(x, y);
            a += Character.charCount(x);
            b += Character.charCount(y);
        }
        return Integer.compare(left.length() - a, right.length() - b);
    }

    public static int length(String text) { return text.codePointCount(0, text.length()); }
    public static long encodedLength(String text) {
        long length = 0;
        for (int i = 0; i < text.length();) {
            int c = text.codePointAt(i); i += Character.charCount(c);
            length += c < 0x80 ? 1 : c < 0x800 ? 2 : c < 0x10000 ? 3 : 4;
        }
        return length;
    }
}
