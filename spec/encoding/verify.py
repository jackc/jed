#!/usr/bin/env python3
# Verify spec/encoding/integers.toml against the encoding rules in
# spec/design/encoding.md. Independent reference encoder — recomputes the bytes
# from scratch and checks the three invariants (round-trip, byte-exactness, order)
# rather than trusting the file. Test-time only (CLAUDE.md §5).
#
#   python3 spec/encoding/verify.py
#
# Exit 0 = all vectors conform; nonzero = mismatch (prints the offending case).
import sys, tomllib, pathlib

WIDTH = {"int16": 2, "int32": 4, "int64": 8}

def enc_bare(v, width):
    return (v + (1 << (width * 8 - 1))).to_bytes(width, "big")

def dec_bare(b, width):
    return int.from_bytes(b, "big") - (1 << (width * 8 - 1))

def enc_nullable(case, width):
    if case.get("null"):
        return bytes([0x00])
    return bytes([0x01]) + enc_bare(case["value"], width)

def invert(b):
    return bytes(x ^ 0xFF for x in b)

def fail(msg):
    print(f"FAIL: {msg}")
    sys.exit(1)

def check_order(label, rows):
    # rows: list of (human, bytes) in the order listed; must be strictly increasing.
    prev_h = prev_b = None
    for h, b in rows:
        if prev_b is not None and not (prev_b < b):
            fail(f"{label}: order not strictly increasing at {prev_h!r} -> {h!r} "
                 f"({prev_b.hex()} !< {b.hex()})")
        prev_h, prev_b = h, b

def main():
    path = pathlib.Path(__file__).with_name("integers.toml")
    data = tomllib.loads(path.read_text())
    checked = 0

    for group in data.get("bare", []):
        t = group["type"]; w = WIDTH[t]
        rows = []
        for c in group["cases"]:
            v = c["value"]; want = bytes.fromhex(c["bytes"])
            got = enc_bare(v, w)
            if got != want:
                fail(f"bare {t} value={v}: encode={got.hex()} want={c['bytes']}")
            if dec_bare(want, w) != v:
                fail(f"bare {t} value={v}: round-trip mismatch")
            rows.append((v, want)); checked += 1
        check_order(f"bare {t}", rows)

    for group in data.get("nullable", []):
        t = group["type"]; w = WIDTH[t]
        rows = []
        for c in group["cases"]:
            want = bytes.fromhex(c["bytes"])
            got = enc_nullable(c, w)
            if got != want:
                lbl = "NULL" if c.get("null") else c.get("value")
                fail(f"nullable {t} {lbl}: encode={got.hex()} want={c['bytes']}")
            rows.append(("NULL" if c.get("null") else c.get("value"), want)); checked += 1
        check_order(f"nullable {t}", rows)

    for group in data.get("descending", []):
        t = group["type"]; w = WIDTH[t]
        rows = []
        for c in group["cases"]:
            want = bytes.fromhex(c["bytes"])
            got = invert(enc_nullable(c, w))   # descending = invert(ascending)
            if got != want:
                lbl = "NULL" if c.get("null") else c.get("value")
                fail(f"descending {t} {lbl}: encode={got.hex()} want={c['bytes']}")
            rows.append(("NULL" if c.get("null") else c.get("value"), want)); checked += 1
        check_order(f"descending {t}", rows)

    print(f"OK: {checked} vectors verified (round-trip + byte-exact + order)")

if __name__ == "__main__":
    main()
