import { Decimal } from "./decimal.ts";

// Round a finite decimal spelling directly to binary32. Number followed by fround usually
// suffices, except when the binary64 conversion lands exactly on a binary32 midpoint. Compare
// the original spelling with that exact midpoint to avoid rounding twice. This also handles
// the zero/subnormal and largest-finite/infinity boundaries. The caller owns range errors.
export function float32FromText(text: string): number {
  const parsed = Number(text);
  const rounded = Math.fround(parsed);
  if (!Number.isFinite(parsed) || parsed === rounded) return rounded;
  const magnitude = Math.abs(parsed);
  const candidate = Math.abs(rounded);
  const bits = new DataView(new ArrayBuffer(4));
  bits.setFloat32(0, candidate);
  const candidateBits = bits.getUint32(0);
  bits.setUint32(0, candidateBits + (magnitude > candidate ? 1 : -1));
  const neighbor = bits.getFloat32(0);
  const low = Math.min(candidate, neighbor);
  const high = Math.max(candidate, neighbor);
  // Infinity's adjacent mathematical value is 2^128 for purposes of the overflow midpoint.
  const midpoint = (low + (Number.isFinite(high) ? high : 2 ** 128)) / 2;
  if (magnitude !== midpoint) return rounded;
  const exactMidpoint = Decimal.exactFromFloat64(midpoint).render();
  const cmp = compareDecimalMagnitude(text, exactMidpoint);
  const result = cmp < 0 ? low : cmp > 0 ? high : candidate;
  return parsed < 0 ? -result : result;
}

// Normalize the decimal exponent and significant digits without imposing numeric's precision
// caps on float input. Comparing equal-length significands needs only implicit trailing zeros.
function compareDecimalMagnitude(a: string, b: string): number {
  const parts = (s: string): [string, number] => {
    const [mantissa, exponent = "0"] = s.replace(/^[+-]/, "").toLowerCase().split("e");
    const point = mantissa!.indexOf(".");
    const digits = mantissa!.replace(".", "");
    const leading = digits.length - digits.replace(/^0+/, "").length;
    return [
      digits.slice(leading).replace(/0+$/, ""),
      Number(exponent) + (point < 0 ? digits.length : point) - leading,
    ];
  };
  const [ad, ae] = parts(a);
  const [bd, be] = parts(b);
  if (ae !== be) return ae < be ? -1 : 1;
  for (let i = 0; i < Math.max(ad.length, bd.length); i++) {
    const ac = ad[i] ?? "0";
    const bc = bd[i] ?? "0";
    if (ac !== bc) return ac < bc ? -1 : 1;
  }
  return 0;
}
