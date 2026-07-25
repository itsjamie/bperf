import { createReadStream } from "node:fs";
import { Writable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { createGunzip } from "node:zlib";

const MAX_PROTOBUF_FIELD_NUMBER = (1 << 29) - 1;
const MAX_PROTOBUF_MESSAGE_BYTES = 64 * 1024 * 1024;
const NODE_ID_FIELD = 1;
const NODE_SIZE_FIELD = 4;
const INVALID_SNAPSHOT_PREFIX =
  "Firefox emitted an invalid .fxsnapshot: ";

// An .fxsnapshot is a gzip stream of varint-length-prefixed protobuf messages:
// Metadata first, then Nodes. CoreDump.proto assigns fields 1 and 4 to Node.id
// and Node.size; the remaining graph data can be skipped by protobuf wire type.
interface Varint {
  value: bigint;
  nextOffset: number;
}

function invalidSnapshot(reason: string): Error {
  return new Error(`${INVALID_SNAPSHOT_PREFIX}${reason}`);
}

function readVarint(
  buffer: Buffer,
  offset: number,
  limit = buffer.length,
): Varint {
  let value = 0n;
  for (let index = 0; index < 10; index += 1) {
    if (offset >= limit) {
      throw invalidSnapshot("truncated protobuf varint");
    }
    const byte = buffer[offset];
    offset += 1;
    if (index === 9 && byte > 1) {
      throw invalidSnapshot("protobuf varint exceeds 64 bits");
    }
    value |= BigInt(byte & 0x7f) << BigInt(index * 7);
    if ((byte & 0x80) === 0) {
      return { value, nextOffset: offset };
    }
  }
  throw invalidSnapshot("protobuf varint exceeds 64 bits");
}

function boundedLength(
  value: bigint,
  remaining: number,
  description: string,
): number {
  if (value > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw invalidSnapshot(`${description} exceeds JavaScript's safe range`);
  }
  const length = Number(value);
  if (length > remaining) {
    throw invalidSnapshot(`truncated ${description}`);
  }
  return length;
}

function readTag(
  buffer: Buffer,
  offset: number,
  limit: number,
): {
  fieldNumber: number;
  wireType: number;
  nextOffset: number;
} {
  const tag = readVarint(buffer, offset, limit);
  const fieldNumber = Number(tag.value >> 3n);
  const wireType = Number(tag.value & 0x07n);
  if (
    fieldNumber <= 0 ||
    fieldNumber > MAX_PROTOBUF_FIELD_NUMBER
  ) {
    throw invalidSnapshot("invalid protobuf field number");
  }
  return {
    fieldNumber,
    wireType,
    nextOffset: tag.nextOffset,
  };
}

function skipField(
  buffer: Buffer,
  offset: number,
  limit: number,
  fieldNumber: number,
  wireType: number,
): number {
  switch (wireType) {
    case 0:
      return readVarint(buffer, offset, limit).nextOffset;
    case 1:
      if (limit - offset < 8) {
        throw invalidSnapshot("truncated fixed64 protobuf field");
      }
      return offset + 8;
    case 2: {
      const encodedLength = readVarint(buffer, offset, limit);
      const length = boundedLength(
        encodedLength.value,
        limit - encodedLength.nextOffset,
        "length-delimited protobuf field",
      );
      return encodedLength.nextOffset + length;
    }
    case 3: {
      let cursor = offset;
      while (cursor < limit) {
        const tag = readTag(buffer, cursor, limit);
        cursor = tag.nextOffset;
        if (tag.wireType === 4) {
          if (tag.fieldNumber !== fieldNumber) {
            throw invalidSnapshot("mismatched protobuf group");
          }
          return cursor;
        }
        cursor = skipField(
          buffer,
          cursor,
          limit,
          tag.fieldNumber,
          tag.wireType,
        );
      }
      throw invalidSnapshot("unterminated protobuf group");
    }
    case 4:
      throw invalidSnapshot("unexpected protobuf end-group");
    case 5:
      if (limit - offset < 4) {
        throw invalidSnapshot("truncated fixed32 protobuf field");
      }
      return offset + 4;
    default:
      throw invalidSnapshot(`unsupported protobuf wire type ${wireType}`);
  }
}

function nodeSize(message: Buffer): number {
  let offset = 0;
  let hasId = false;
  let size: bigint | undefined;

  while (offset < message.length) {
    const tag = readTag(message, offset, message.length);
    offset = tag.nextOffset;
    if (
      tag.fieldNumber === NODE_ID_FIELD &&
      tag.wireType === 0
    ) {
      const id = readVarint(message, offset, message.length);
      hasId = true;
      offset = id.nextOffset;
      continue;
    }
    if (
      tag.fieldNumber === NODE_SIZE_FIELD &&
      tag.wireType === 0
    ) {
      const encodedSize = readVarint(
        message,
        offset,
        message.length,
      );
      size = encodedSize.value;
      offset = encodedSize.nextOffset;
      continue;
    }
    offset = skipField(
      message,
      offset,
      message.length,
      tag.fieldNumber,
      tag.wireType,
    );
  }

  if (!hasId) {
    throw invalidSnapshot("heap node has no ID");
  }
  if (size === undefined) {
    throw invalidSnapshot("heap node has no size");
  }
  if (size > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw invalidSnapshot("heap node has an invalid size");
  }
  return Number(size);
}

class HeapSnapshotMessages {
  #encodedLength = 0;
  #lengthByteCount = 0;
  #messageLength: number | undefined;
  #messageRemaining = 0;
  #messageChunks: Buffer[] = [];
  #messageIndex = 0;
  #nodeCount = 0;
  #total = 0;

  consume(chunk: Buffer): void {
    let offset = 0;
    while (offset < chunk.length) {
      if (this.#messageLength === undefined) {
        this.#consumeLengthByte(chunk[offset]);
        offset += 1;
        continue;
      }

      const length = Math.min(
        this.#messageRemaining,
        chunk.length - offset,
      );
      if (length > 0) {
        this.#messageChunks.push(
          chunk.subarray(offset, offset + length),
        );
        this.#messageRemaining -= length;
        offset += length;
      }
      if (this.#messageRemaining === 0) {
        this.#finishMessage();
      }
    }
  }

  #consumeLengthByte(byte: number): void {
    if (
      this.#lengthByteCount === 4 &&
      (byte & 0xf0) !== 0
    ) {
      throw invalidSnapshot(
        "heap snapshot message length exceeds 32 bits",
      );
    }
    this.#encodedLength +=
      (byte & 0x7f) * 2 ** (this.#lengthByteCount * 7);
    this.#lengthByteCount += 1;

    if ((byte & 0x80) !== 0) {
      if (this.#lengthByteCount === 5) {
        throw invalidSnapshot(
          "heap snapshot message length exceeds 32 bits",
        );
      }
      return;
    }
    if (this.#encodedLength === 0) {
      throw invalidSnapshot("heap snapshot message is empty");
    }
    if (this.#encodedLength > MAX_PROTOBUF_MESSAGE_BYTES) {
      throw invalidSnapshot(
        "heap snapshot message exceeds Firefox's protobuf limit",
      );
    }

    this.#messageLength = this.#encodedLength;
    this.#messageRemaining = this.#encodedLength;
  }

  #finishMessage(): void {
    const length = this.#messageLength;
    if (length === undefined) {
      throw invalidSnapshot("heap snapshot message has no length");
    }
    const message =
      this.#messageChunks.length === 1
        ? this.#messageChunks[0]
        : Buffer.concat(this.#messageChunks, length);

    if (this.#messageIndex > 0) {
      const size = nodeSize(message);
      if (this.#total > Number.MAX_SAFE_INTEGER - size) {
        throw invalidSnapshot(
          "total heap size exceeds JavaScript's safe range",
        );
      }
      this.#total += size;
      this.#nodeCount += 1;
    }
    this.#messageIndex += 1;
    this.#encodedLength = 0;
    this.#lengthByteCount = 0;
    this.#messageLength = undefined;
    this.#messageRemaining = 0;
    this.#messageChunks = [];
  }

  finish(): number {
    if (
      this.#messageLength !== undefined ||
      this.#lengthByteCount > 0
    ) {
      throw invalidSnapshot("truncated heap snapshot message");
    }
    if (this.#messageIndex === 0) {
      throw invalidSnapshot("heap snapshot contains no metadata");
    }
    if (this.#nodeCount === 0 || this.#total <= 0) {
      throw invalidSnapshot(
        "heap snapshot contains no live heap nodes",
      );
    }
    return this.#total;
  }
}

export async function firefoxHeapSnapshotLiveBytes(
  filePath: string,
): Promise<number> {
  const messages = new HeapSnapshotMessages();
  try {
    await pipeline(
      createReadStream(filePath),
      createGunzip(),
      new Writable({
        write(chunk: Buffer, _encoding, callback) {
          try {
            messages.consume(chunk);
            callback();
          } catch (error) {
            callback(
              error instanceof Error
                ? error
                : new Error(String(error)),
            );
          }
        },
      }),
    );
  } catch (error) {
    if (
      error instanceof Error &&
      error.message.startsWith(INVALID_SNAPSHOT_PREFIX)
    ) {
      throw error;
    }
    const reason = error instanceof Error ? error.message : String(error);
    throw invalidSnapshot(`gzip decoding failed: ${reason}`);
  }
  return messages.finish();
}
