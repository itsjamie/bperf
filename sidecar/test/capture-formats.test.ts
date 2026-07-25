import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { gzipSync } from "node:zlib";

import { chromiumArtifactFormat } from "../src/engines/chromium.ts";
import { firefoxArtifactFormat } from "../src/engines/firefox-artifacts.ts";
import { webkitArtifactFormat } from "../src/engines/webkit.ts";

const fixtureRoot = path.join(
  import.meta.dirname,
  "fixtures",
  "captures",
);
const targetUrl = "http://127.0.0.1:4317/";

test("Chromium golden capture preserves metrics and flamegraph shape", () => {
  const root = path.join(fixtureRoot, "chromium");
  const profile = readJson<
    Parameters<typeof chromiumArtifactFormat.cpuActiveMilliseconds>[0]
  >(path.join(root, "cpu.json"));
  const output = temporaryOutput("chromium");

  assert.equal(
    chromiumArtifactFormat.cpuActiveMilliseconds(profile, targetUrl),
    3,
  );
  assert.equal(
    chromiumArtifactFormat.liveHeapBytes(path.join(root, "heap.json")),
    96,
  );
  chromiumArtifactFormat.writeFlamegraph(profile, output, targetUrl);
  assert.deepEqual(readJson(output), readJson(path.join(root, "flamegraph.json")));
});

test("Firefox golden capture preserves metrics and flamegraph shape", async () => {
  const root = path.join(fixtureRoot, "firefox");
  const profile = firefoxArtifactFormat.parseProfile(
    fs.readFileSync(path.join(root, "cpu.json"), "utf8"),
  );
  const output = temporaryOutput("firefox");

  assert.equal(
    firefoxArtifactFormat.cpuActiveMilliseconds(profile, targetUrl),
    8,
  );
  assert.equal(
    await firefoxArtifactFormat.liveHeapBytes(
      path.join(root, "heap.fxsnapshot"),
    ),
    301,
  );
  firefoxArtifactFormat.writeFlamegraph(profile, output, targetUrl);
  assert.deepEqual(readJson(output), readJson(path.join(root, "flamegraph.json")));
});

test("Firefox heap snapshots allow zero-sized nodes", async () => {
  const heap = temporaryOutput("firefox-zero-sized-node");
  fs.writeFileSync(
    heap,
    gzipSync(Buffer.from("02082a04080120000408022001", "hex")),
  );

  assert.equal(await firefoxArtifactFormat.liveHeapBytes(heap), 1);
});

test("Firefox heap snapshots are read without whole-file buffering", async () => {
  const heap = temporaryOutput("firefox-streamed-heap");
  fs.writeFileSync(
    heap,
    gzipSync(Buffer.from("02082a0408012001", "hex")),
  );
  const readFileSync = fs.readFileSync;
  fs.readFileSync = ((filePath, ...args) => {
    if (filePath === heap) {
      throw new Error("heap snapshot was read as one buffer");
    }
    return readFileSync(filePath, ...args);
  }) as typeof fs.readFileSync;

  try {
    assert.equal(await firefoxArtifactFormat.liveHeapBytes(heap), 1);
  } finally {
    fs.readFileSync = readFileSync;
  }
});

test("Firefox heap snapshots parse messages across decompression chunks", async () => {
  const heap = temporaryOutput("firefox-chunked-heap");
  const typeName = Buffer.alloc(128 * 1024, 97);
  const node = Buffer.concat([
    Buffer.from([0x08, 0x01, 0x20, 0x01, 0x12]),
    encodeVarint(typeName.length),
    typeName,
  ]);
  fs.writeFileSync(
    heap,
    gzipSync(
      Buffer.concat([
        frameMessage(Buffer.from([0x08, 0x2a])),
        frameMessage(node),
      ]),
    ),
  );

  assert.equal(await firefoxArtifactFormat.liveHeapBytes(heap), 1);
});

test("Firefox heap snapshots reject truncated gzip streams", async () => {
  const heap = temporaryOutput("firefox-truncated-gzip");
  const snapshot = gzipSync(
    Buffer.from("02082a0408012001", "hex"),
  );
  fs.writeFileSync(heap, snapshot.subarray(0, snapshot.length - 4));

  await assert.rejects(
    () => firefoxArtifactFormat.liveHeapBytes(heap),
    /gzip decoding failed/,
  );
});

test("Firefox heap snapshots require every node to have a size", async () => {
  const heap = temporaryOutput("firefox-invalid-heap");
  fs.writeFileSync(
    heap,
    gzipSync(Buffer.from("02082a06080112025200", "hex")),
  );

  await assert.rejects(
    () => firefoxArtifactFormat.liveHeapBytes(heap),
    /heap node has no size/,
  );
});

test("WebKit golden capture preserves metrics and flamegraph shape", () => {
  const root = path.join(fixtureRoot, "webkit");
  const profile = readJson<
    Parameters<typeof webkitArtifactFormat.cpuActiveMilliseconds>[0]
  >(path.join(root, "cpu.json"));
  const output = temporaryOutput("webkit");

  assert.equal(
    webkitArtifactFormat.cpuActiveMilliseconds(profile, targetUrl),
    2,
  );
  assert.equal(
    webkitArtifactFormat.liveHeapBytes(
      fs.readFileSync(path.join(root, "heap.json"), "utf8"),
    ),
    96,
  );
  webkitArtifactFormat.writeFlamegraph(profile, output, targetUrl);
  assert.deepEqual(readJson(output), readJson(path.join(root, "flamegraph.json")));
});

function readJson<T = unknown>(filePath: string): T {
  return JSON.parse(fs.readFileSync(filePath, "utf8")) as T;
}

function temporaryOutput(engine: string): string {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), `bperf-${engine}-golden-`));
  return path.join(root, "flamegraph.json");
}

function frameMessage(message: Buffer): Buffer {
  return Buffer.concat([encodeVarint(message.length), message]);
}

function encodeVarint(value: number): Buffer {
  const bytes: number[] = [];
  do {
    let byte = value & 0x7f;
    value = Math.floor(value / 128);
    if (value > 0) byte |= 0x80;
    bytes.push(byte);
  } while (value > 0);
  return Buffer.from(bytes);
}
