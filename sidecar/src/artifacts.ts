import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

import type { ArtifactEvidence, ArtifactKind } from "./contract.ts";

const HASH_BUFFER_BYTES = 64 * 1024;

export function prepareArtifact(
  artifactDirectory: string,
  fileName: string,
): string {
  const root = path.resolve(artifactDirectory);
  fs.mkdirSync(root, { recursive: true });

  const artifactPath = path.resolve(root, fileName);
  const relativePath = path.relative(root, artifactPath);
  if (
    relativePath === ".." ||
    relativePath.startsWith(`..${path.sep}`) ||
    path.isAbsolute(relativePath)
  ) {
    throw new Error(`Artifact path escaped its root: ${fileName}`);
  }

  fs.mkdirSync(path.dirname(artifactPath), { recursive: true });
  fs.rmSync(artifactPath, { force: true });
  return artifactPath;
}

export function describeArtifact(
  root: string,
  kind: ArtifactKind,
  filePath: string,
  format: string,
): ArtifactEvidence {
  const digest = crypto.createHash("sha256");
  const buffer = Buffer.allocUnsafe(HASH_BUFFER_BYTES);
  const descriptor = fs.openSync(filePath, "r");
  let sizeBytes = 0;

  try {
    while (true) {
      const count = fs.readSync(
        descriptor,
        buffer,
        0,
        buffer.length,
        null,
      );
      if (count === 0) break;
      sizeBytes += count;
      digest.update(buffer.subarray(0, count));
    }
  } finally {
    fs.closeSync(descriptor);
  }

  if (sizeBytes === 0) {
    throw new Error(`Empty ${kind} artifact: ${filePath}`);
  }

  return {
    kind,
    path: path.relative(root, filePath).replaceAll("\\", "/"),
    size_bytes: sizeBytes,
    sha256: digest.digest("hex"),
    format,
  };
}
