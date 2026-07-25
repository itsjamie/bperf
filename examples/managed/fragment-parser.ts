export async function parseFragmentStream(
  stream: ReadableStream<Uint8Array>,
): Promise<{ byteLength: number; checksum: number }> {
  const reader = stream.getReader();
  const chunks: Uint8Array[] = [];
  let byteLength = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    byteLength += value.byteLength;
  }

  const bytes = new Uint8Array(byteLength);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }

  const { checksumBytes } = await import("./fragment-checksum.ts");
  return { byteLength, checksum: checksumBytes(bytes) };
}
