package org.jed;

import java.io.ByteArrayOutputStream;
import java.util.HexFormat;

/** PostgreSQL bytea encoding helpers, including its permissive padded base64 groups. */
final class BinaryText {
    private BinaryText() {}
    private static final String ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    static String encode(byte[] bytes, String format) {
        return switch (format(format)) {
            case "hex" -> HexFormat.of().formatHex(bytes);
            case "escape" -> {
                StringBuilder output = new StringBuilder();
                for (byte raw : bytes) {
                    int b = raw & 255;
                    if (b == '\\') output.append("\\\\");
                    else if (b == 0 || b >= 128) output.append('\\').append((char) ('0' + (b >>> 6)))
                        .append((char) ('0' + ((b >>> 3) & 7))).append((char) ('0' + (b & 7)));
                    else output.append((char) b);
                }
                yield output.toString();
            }
            case "base64" -> {
                StringBuilder output = new StringBuilder();
                int column = 0;
                for (int i = 0; i < bytes.length; i += 3) {
                    int remaining = bytes.length - i;
                    int bits = (bytes[i] & 255) << 16;
                    if (remaining >= 2) bits |= (bytes[i + 1] & 255) << 8;
                    if (remaining >= 3) bits |= bytes[i + 2] & 255;
                    output.append(ALPHABET.charAt(bits >>> 18)).append(ALPHABET.charAt((bits >>> 12) & 63))
                        .append(remaining >= 2 ? ALPHABET.charAt((bits >>> 6) & 63) : '=')
                        .append(remaining >= 3 ? ALPHABET.charAt(bits & 63) : '=');
                    column += 4;
                    if (remaining >= 3 && column == 76) { output.append('\n'); column = 0; }
                }
                yield output.toString();
            }
            default -> throw new IllegalStateException();
        };
    }

    static byte[] decode(String text, String format) {
        ByteArrayOutputStream output = new ByteArrayOutputStream();
        switch (format(format)) {
            case "hex": {
                int high = -1;
                for (int i = 0; i < text.length();) {
                    int c = text.codePointAt(i); i += Character.charCount(c);
                    if (whitespace(c) && high < 0) continue;
                    int digit = c >= '0' && c <= '9' ? c - '0' : c >= 'a' && c <= 'f' ? c - 'a' + 10 : c >= 'A' && c <= 'F' ? c - 'A' + 10 : -1;
                    if (digit < 0) throw new SqlException("22023", "invalid hexadecimal digit: \"" + new String(Character.toChars(c)) + "\"");
                    if (high < 0) high = digit;
                    else { output.write(high * 16 + digit); high = -1; }
                }
                if (high >= 0) throw new SqlException("22023", "invalid hexadecimal data: odd number of digits");
                break;
            }
            case "escape": {
                byte[] input = SqlText.encode(text);
                for (int i = 0; i < input.length; i++) {
                    int b = input[i] & 255;
                    if (b != '\\') output.write(b);
                    else if (i + 1 < input.length && input[i + 1] == '\\') { output.write('\\'); i++; }
                    else if (i + 3 < input.length && input[i + 1] >= '0' && input[i + 1] <= '3'
                            && input[i + 2] >= '0' && input[i + 2] <= '7' && input[i + 3] >= '0' && input[i + 3] <= '7') {
                        output.write((input[i + 1] - '0') * 64 + (input[i + 2] - '0') * 8 + input[i + 3] - '0');
                        i += 3;
                    } else throw new SqlException("22P02", "invalid input syntax for type bytea");
                }
                break;
            }
            case "base64": {
                int position = 0, bits = 0, outputBytes = 3;
                for (int i = 0; i < text.length();) {
                    int c = text.codePointAt(i); i += Character.charCount(c);
                    if (whitespace(c)) continue;
                    int value = ALPHABET.indexOf(c);
                    if (c == '=') {
                        if (position < 2) throw new SqlException("22023", "unexpected \"=\" while decoding base64 sequence");
                        if (outputBytes == 3) outputBytes = position - 1;
                        value = 0;
                    } else if (value < 0) {
                        throw new SqlException("22023", "invalid symbol \"" + new String(Character.toChars(c)) + "\" found while decoding base64 sequence");
                    }
                    if (outputBytes != 3) value = 0;
                    bits = (bits << 6) | value;
                    if (++position == 4) {
                        output.write((bits >>> 16) & 255);
                        if (outputBytes > 1) output.write((bits >>> 8) & 255);
                        if (outputBytes > 2) output.write(bits & 255);
                        position = 0; bits = 0; outputBytes = 3;
                    }
                }
                if (position != 0) throw new SqlException("22023", "invalid base64 end sequence");
                break;
            }
            default: throw new IllegalStateException();
        }
        return output.toByteArray();
    }

    private static String format(String original) {
        StringBuilder lowered = new StringBuilder(original.length());
        for (int i = 0; i < original.length(); i++) {
            char c = original.charAt(i);
            lowered.append(c >= 'A' && c <= 'Z' ? (char) (c + 32) : c);
        }
        String result = lowered.toString();
        if (!result.equals("hex") && !result.equals("base64") && !result.equals("escape")) {
            throw new SqlException("22023", "unrecognized encoding: \"" + original + "\"");
        }
        return result;
    }
    private static boolean whitespace(int c) { return c == ' ' || c == '\t' || c == '\n' || c == '\r'; }
}
