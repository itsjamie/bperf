export function checksumBytes(bytes: Uint8Array): number {
  let checksum = 2166136261;
  for (let round = 0; round < 4_000; round += 1) {
    for (const byte of bytes) {
      checksum = Math.imul(checksum ^ byte ^ (round & 7), 16777619) >>> 0;
    }
  }
  return checksum;
}
