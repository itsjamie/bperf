import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import { firefoxHeapSnapshotLiveBytes } from "./firefox-heap-snapshot.ts";

async function waitForSnapshot(
  filePath: string,
  timeoutMs = 10_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (fs.existsSync(filePath) && fs.statSync(filePath).size > 0) {
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`Firefox did not write heap snapshot ${filePath}`);
}

function linkOrCopySnapshot(
  sourcePath: string,
  destinationPath: string,
): void {
  try {
    fs.linkSync(sourcePath, destinationPath);
  } catch (error) {
    const code = (error as NodeJS.ErrnoException).code;
    if (
      !["EACCES", "ENOSYS", "ENOTSUP", "EPERM", "EXDEV"].includes(
        code ?? "",
      )
    ) {
      throw error;
    }
    fs.copyFileSync(sourcePath, destinationPath);
  }
}

function removeSnapshotIfReleased(filePath: string): boolean {
  try {
    fs.rmSync(filePath, { force: true });
    return true;
  } catch (error) {
    const code = (error as NodeJS.ErrnoException).code;
    if (["EBUSY", "EPERM"].includes(code ?? "")) return false;
    throw error;
  }
}

async function removeSnapshot(
  filePath: string,
  timeoutMs = 10_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!removeSnapshotIfReleased(filePath)) {
    if (Date.now() >= deadline) {
      throw new Error(
        `Firefox did not release heap snapshot ${filePath}`,
      );
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}

/**
 * Retains native Firefox snapshots at caller-selected paths and reports the
 * live-byte scalar derived from each retained artifact. Call close only after
 * the owning Firefox browser exits so locked temporary source names can clear.
 */
export class FirefoxHeapSnapshotFiles {
  readonly #sources = new Set<string>();

  async capture(
    snapshotId: string,
    destinationPath: string,
  ): Promise<number> {
    if (!/^\d+(?:-\d+)?$/.test(snapshotId)) {
      throw new Error(
        "Firefox MemoryActor returned an invalid heap snapshot ID",
      );
    }
    const sourcePath = path.join(
      os.tmpdir(),
      `${snapshotId}.fxsnapshot`,
    );
    this.#sources.add(sourcePath);
    await waitForSnapshot(sourcePath);

    // Content-process snapshots can retain their source handle until Firefox
    // exits on Windows. A hard link avoids duplicating the snapshot while the
    // retained browser lane is alive; its source name is removed on shutdown.
    linkOrCopySnapshot(sourcePath, destinationPath);
    const liveBytes =
      await firefoxHeapSnapshotLiveBytes(destinationPath);
    if (removeSnapshotIfReleased(sourcePath)) {
      this.#sources.delete(sourcePath);
    }
    return liveBytes;
  }

  async close(): Promise<void> {
    const sources = [...this.#sources];
    const results = await Promise.allSettled(
      sources.map((sourcePath) => removeSnapshot(sourcePath)),
    );
    const failures: unknown[] = [];
    for (const [index, result] of results.entries()) {
      if (result.status === "fulfilled") {
        this.#sources.delete(sources[index]);
      } else {
        failures.push(result.reason);
      }
    }
    if (failures.length > 0) {
      throw new AggregateError(
        failures,
        "Firefox did not release one or more heap snapshots",
      );
    }
  }
}
