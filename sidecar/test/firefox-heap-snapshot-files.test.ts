import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { FirefoxHeapSnapshotFiles } from "../src/engines/firefox-heap-snapshot-files.ts";
import { firefoxHeapSnapshotLiveBytes } from "../src/engines/firefox-heap-snapshot.ts";

const fixturePath = fileURLToPath(
  new URL(
    "./fixtures/captures/firefox/heap.fxsnapshot",
    import.meta.url,
  ),
);

test("Firefox snapshot files retain evidence and release the source name", async (t) => {
  const snapshotId = `${process.pid}-${process.hrtime.bigint()}`;
  const sourcePath = path.join(
    os.tmpdir(),
    `${snapshotId}.fxsnapshot`,
  );
  const outputRoot = fs.mkdtempSync(
    path.join(os.tmpdir(), "bperf-firefox-snapshot-files-"),
  );
  const destinationPath = path.join(outputRoot, "heap.fxsnapshot");
  t.after(() => {
    fs.rmSync(sourcePath, { force: true });
    fs.rmSync(outputRoot, { force: true, recursive: true });
  });
  fs.copyFileSync(fixturePath, sourcePath);
  const snapshots = new FirefoxHeapSnapshotFiles();

  assert.equal(
    await snapshots.capture(snapshotId, destinationPath),
    301,
  );
  assert.equal(fs.existsSync(sourcePath), false);
  assert.equal(
    await firefoxHeapSnapshotLiveBytes(destinationPath),
    301,
  );
  await snapshots.close();
});

test("Firefox snapshot files reject IDs that could escape the temp directory", async () => {
  const snapshots = new FirefoxHeapSnapshotFiles();

  await assert.rejects(
    () => snapshots.capture("../heap", "unused"),
    /invalid heap snapshot ID/,
  );
  await snapshots.close();
});
