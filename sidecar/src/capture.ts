import { ARTIFACT_KINDS, ENGINE_IDS, RUNTIME_ANCHOR } from "./contract.ts";
import type {
  CaptureEvidence,
  EngineAdapter,
  EngineCapture,
  EngineId,
} from "./contract.ts";
import { captureChromium } from "./engines/chromium.ts";
import { captureFirefox } from "./engines/firefox.ts";
import { captureWebkit } from "./engines/webkit.ts";
import { runtimeInfo } from "./playwright-runtime.ts";

const adapters = {
  chromium: captureChromium,
  firefox: captureFirefox,
  webkit: captureWebkit,
} satisfies Record<EngineId, EngineAdapter>;

export function isSupportedEngine(value: unknown): value is EngineId {
  return (
    typeof value === "string" &&
    (ENGINE_IDS as readonly string[]).includes(value)
  );
}

function validateCapture(engine: EngineId, capture: EngineCapture): void {
  if (!Number.isSafeInteger(capture.browser.root_pid)) {
    throw new Error(`${engine} did not expose a browser root PID`);
  }
  if (
    capture.browser.root_pid <= 0 ||
    capture.browser.executable_path.length === 0 ||
    capture.browser.version.length === 0
  ) {
    throw new Error(`${engine} returned incomplete browser identity`);
  }
  if (
    capture.anchor.workload !== RUNTIME_ANCHOR.workload ||
    capture.anchor.wall_ms.length !== RUNTIME_ANCHOR.samples ||
    !capture.anchor.wall_ms.every(
      (sample) => Number.isFinite(sample) && sample > 0,
    ) ||
    !Number.isSafeInteger(capture.anchor.batch_size) ||
    capture.anchor.batch_size < 1 ||
    capture.anchor.batch_size > RUNTIME_ANCHOR.maxBatchSize ||
    !Number.isSafeInteger(capture.anchor.checksum)
  ) {
    throw new Error(`${engine} returned invalid runtime anchor evidence`);
  }

  const counts = new Map<(typeof ARTIFACT_KINDS)[number], number>(
    ARTIFACT_KINDS.map((kind) => [kind, 0] as const),
  );
  for (const artifact of capture.artifacts) {
    const count = counts.get(artifact.kind);
    if (count === undefined) {
      throw new Error(`${engine} returned an unknown artifact kind`);
    }
    counts.set(artifact.kind, count + 1);
  }

  const invalid = [...counts].filter(([, count]) => count !== 1);
  if (
    invalid.length > 0 ||
    capture.artifacts.length !== ARTIFACT_KINDS.length
  ) {
    throw new Error(
      `${engine} must return one CPU profile, heap capture, and flamegraph`,
    );
  }
}

export async function captureEngine(
  engine: EngineId,
  artifactDirectory: string,
): Promise<CaptureEvidence> {
  const captured = await adapters[engine](artifactDirectory);
  validateCapture(engine, captured);

  return {
    engine,
    runtime: runtimeInfo(),
    ...captured,
    capabilities: {
      isolated_launch: true,
      process_root: true,
      cpu_profile: true,
      js_heap: true,
      flamegraph: true,
    },
  };
}
