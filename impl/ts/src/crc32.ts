// CRC-32/IEEE backend seam. The file-format contract fixes the reflected polynomial, initialization,
// finalization, coverage, and result bytes; each runtime may select faster implementation machinery.
// Browser/core-only entry paths keep the safe slicing-by-8 backend below. The Node host installs its
// standard-library backend from crc32_node.ts before exposing any database operation.

const CRC32_IEEE_POLYNOMIAL = 0xedb88320;

export type Crc32Backend = (previous: number, data: Uint8Array) => number;

function makeCrc32Tables(): readonly Uint32Array[] {
  const tables = Array.from({ length: 8 }, () => new Uint32Array(256));
  for (let i = 0; i < 256; i++) {
    let crc = i;
    for (let bit = 0; bit < 8; bit++) {
      const mask = -(crc & 1);
      crc = (crc >>> 1) ^ (CRC32_IEEE_POLYNOMIAL & mask);
    }
    tables[0]![i] = crc >>> 0;
  }
  for (let slice = 1; slice < 8; slice++) {
    for (let i = 0; i < 256; i++) {
      const crc = tables[slice - 1]![i]!;
      tables[slice]![i] = ((crc >>> 8) ^ tables[0]![crc & 0xff]!) >>> 0;
    }
  }
  return tables;
}

const CRC32_TABLES = makeCrc32Tables();

// crc32SlicingBy8 extends a finalized CRC-32/IEEE checksum. Passing 0 starts a fresh checksum;
// crc32SlicingBy8(crc32SlicingBy8(0, a), b) is the checksum of a‖b. Explicit little-endian byte
// assembly implements the reflected polynomial without depending on host byte order.
export function crc32SlicingBy8(previous: number, data: Uint8Array): number {
  let crc = (previous ^ 0xffffffff) >>> 0;
  let offset = 0;
  while (offset + 8 <= data.length) {
    const first =
      (crc ^
        (data[offset]! |
          (data[offset + 1]! << 8) |
          (data[offset + 2]! << 16) |
          (data[offset + 3]! << 24))) >>>
      0;
    const second =
      (data[offset + 4]! |
        (data[offset + 5]! << 8) |
        (data[offset + 6]! << 16) |
        (data[offset + 7]! << 24)) >>>
      0;
    crc =
      (CRC32_TABLES[7]![first & 0xff]! ^
        CRC32_TABLES[6]![(first >>> 8) & 0xff]! ^
        CRC32_TABLES[5]![(first >>> 16) & 0xff]! ^
        CRC32_TABLES[4]![first >>> 24]! ^
        CRC32_TABLES[3]![second & 0xff]! ^
        CRC32_TABLES[2]![(second >>> 8) & 0xff]! ^
        CRC32_TABLES[1]![(second >>> 16) & 0xff]! ^
        CRC32_TABLES[0]![second >>> 24]!) >>>
      0;
    offset += 8;
  }
  while (offset < data.length) {
    crc = ((crc >>> 8) ^ CRC32_TABLES[0]![(crc ^ data[offset]!) & 0xff]!) >>> 0;
    offset++;
  }
  return (crc ^ 0xffffffff) >>> 0;
}

let backend: Crc32Backend = crc32SlicingBy8;
let backendName = "slicing-by-8";
let backendUsed = false;

// installCrc32Backend is an internal host-initialization seam, not an embedding option. Refuse a late
// or conflicting selection so one process cannot silently change checksum machinery while databases
// are active. All conforming backends produce the same result; the guard makes selection lifecycle
// explicit as well.
export function installCrc32Backend(name: string, selected: Crc32Backend): void {
  if (backendUsed && backend !== selected) {
    throw new Error(`CRC-32 backend cannot change after first use (${backendName} -> ${name})`);
  }
  if (backend !== crc32SlicingBy8 && backend !== selected) {
    throw new Error(`CRC-32 backend already selected (${backendName})`);
  }
  backend = selected;
  backendName = name;
}

export function selectedCrc32Backend(): string {
  return backendName;
}

export function crc32Update(previous: number, data: Uint8Array): number {
  backendUsed = true;
  return backend(previous >>> 0, data) >>> 0;
}

export function crc32Ieee(data: Uint8Array): number {
  return crc32Update(0, data);
}
