import assert from "node:assert/strict";
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  describeArtifact,
  prepareArtifact,
} from "../src/artifacts.ts";

test("describeArtifact returns a relative path, size, and digest", (context) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "bperf-artifact-"));
  context.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const artifactPath = prepareArtifact(root, "capture/heap.bin");
  const bytes = Buffer.from("captured heap");
  fs.writeFileSync(artifactPath, bytes);

  assert.deepEqual(
    describeArtifact(root, "js_heap", artifactPath, "fixture"),
    {
      kind: "js_heap",
      path: "capture/heap.bin",
      size_bytes: bytes.length,
      sha256: crypto.createHash("sha256").update(bytes).digest("hex"),
      format: "fixture",
    },
  );
});

test("prepareArtifact rejects paths outside the artifact root", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "bperf-artifact-"));
  try {
    assert.throws(
      () => prepareArtifact(root, "../escaped.bin"),
      /escaped its root/,
    );
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
