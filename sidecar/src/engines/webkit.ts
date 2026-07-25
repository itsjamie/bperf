import fs from "node:fs";

import { webkit } from "playwright";

import { describeArtifact, prepareArtifact } from "../artifacts.ts";
import type {
  EngineCapture,
  TrialCapture,
  TrialEngineAdapter,
  TrialLane,
} from "../contract.ts";
import {
  browserInfo,
  webkitInspectorSession,
} from "../playwright-runtime.ts";
import { runProbeWorkload } from "../probe-workload.ts";
import { measureRuntimeAnchor } from "../runtime-anchor.ts";
import {
  positiveWeights,
  SpeedscopeBuilder,
} from "../speedscope.ts";
import {
  executeBenchmarkWorkload,
  selectBenchmarkBatchSize,
  settleBenchmarkPage,
  withPreparedTrialPage,
} from "../trial-workload.ts";

interface WebKitFrame {
  name: string;
  url: string;
  line: number;
  column: number;
}

interface WebKitStackTrace {
  timestamp: number;
  stackFrames: WebKitFrame[];
}

interface WebKitProfile {
  samples?: {
    stackTraces?: WebKitStackTrace[];
  };
}

interface WebKitHeapSnapshot {
  nodes?: number[];
}

function targetTraces(
  profile: WebKitProfile,
  targetUrl: string,
): WebKitStackTrace[] {
  return (profile.samples?.stackTraces ?? []).filter((trace) =>
    trace.stackFrames.some((frame) => frame.url.startsWith(targetUrl)),
  );
}

function cpuActiveMilliseconds(
  profile: WebKitProfile,
  targetUrl: string,
): number {
  const sampleCount = targetTraces(profile, targetUrl).length;
  if (sampleCount === 0) {
    throw new Error("WebKit CPU profile has no positive sample duration");
  }
  return sampleCount;
}

function liveHeapBytes(snapshotData: string): number {
  const snapshot = JSON.parse(snapshotData) as WebKitHeapSnapshot;
  if (
    !Array.isArray(snapshot.nodes) ||
    snapshot.nodes.length === 0 ||
    snapshot.nodes.length % 4 !== 0
  ) {
    throw new Error("WebKit emitted an invalid heap snapshot");
  }

  let total = 0;
  for (let index = 1; index < snapshot.nodes.length; index += 4) {
    const size = snapshot.nodes[index];
    if (!Number.isSafeInteger(size) || size < 0) {
      throw new Error("WebKit heap snapshot contains an invalid node size");
    }
    total += size;
  }
  if (!Number.isSafeInteger(total) || total <= 0) {
    throw new Error("WebKit heap snapshot contains no live heap bytes");
  }
  return total;
}

function writeFlamegraph(
  profile: WebKitProfile,
  filePath: string,
  targetUrl?: string,
): void {
  const builder = new SpeedscopeBuilder("WebKit CPU");
  const traces = targetUrl
    ? targetTraces(profile, targetUrl)
    : profile.samples?.stackTraces ?? [];
  const samples = traces.map((trace) =>
    [...trace.stackFrames].reverse().map((frame) =>
      builder.frame({
        name: frame.name,
        file: frame.url,
        line: frame.line,
        col: frame.column,
      }),
    ),
  );
  builder.sampledProfile({
    name: "WebKit renderer JavaScript",
    unit: "seconds",
    samples,
    weights: targetUrl
      ? traces.map(() => 0.001)
      : positiveWeights(
          traces.map((trace) => trace.timestamp),
          0.001,
        ),
  });
  builder.write(filePath);
}

export const webkitArtifactFormat = {
  cpuActiveMilliseconds,
  liveHeapBytes,
  writeFlamegraph,
} as const;

export async function captureWebkit(
  artifactDirectory: string,
): Promise<EngineCapture> {
  const cpuPath = prepareArtifact(artifactDirectory, "webkit.cpu.json");
  const heapPath = prepareArtifact(artifactDirectory, "webkit.heap.json");
  const flamegraphPath = prepareArtifact(
    artifactDirectory,
    "webkit.flamegraph.speedscope.json",
  );
  const browser = await webkit.launch({ headless: true });

  try {
    const identity = browserInfo(browser);
    const context = await browser.newContext();
    const page = await context.newPage();
    const session = webkitInspectorSession(browser);
    const anchor = await measureRuntimeAnchor(page);

    const trackingComplete = new Promise<WebKitProfile>((resolve) => {
      session.once<WebKitProfile>(
        "ScriptProfiler.trackingComplete",
        resolve,
      );
    });
    await session.send("ScriptProfiler.startTracking", {
      includeSamples: true,
    });
    await runProbeWorkload(page);
    await session.send("ScriptProfiler.stopTracking");
    const profile = await trackingComplete;
    await settleBenchmarkPage(page);
    if ((profile.samples?.stackTraces?.length ?? 0) < 10) {
      throw new Error("WebKit CPU profile did not contain enough samples");
    }
    fs.writeFileSync(cpuPath, JSON.stringify(profile));
    webkitArtifactFormat.writeFlamegraph(profile, flamegraphPath);

    await session.send("Heap.enable");
    const heap = await session.send<{ snapshotData?: string }>(
      "Heap.snapshot",
    );
    if (!heap.snapshotData) {
      throw new Error("WebKit heap snapshot returned no data");
    }
    fs.writeFileSync(heapPath, heap.snapshotData);

    return {
      browser: identity,
      anchor,
      artifacts: [
        describeArtifact(
          artifactDirectory,
          "cpu_profile",
          cpuPath,
          "WebKit ScriptProfiler JSON",
        ),
        describeArtifact(
          artifactDirectory,
          "js_heap",
          heapPath,
          "WebKit Heap snapshot JSON",
        ),
        describeArtifact(
          artifactDirectory,
          "flamegraph",
          flamegraphPath,
          "Speedscope sampled profile",
        ),
      ],
    };
  } finally {
    await browser.close({ reason: "bperf doctor complete" });
  }
}

async function openWebkitTrialLane(): Promise<
  TrialLane<TrialCapture>
> {
  const browser = await webkit.launch({
    headless: true,
  });
  const identity = browserInfo(browser);
  return {
    async capture(request) {
      const cpuPath = prepareArtifact(
        request.artifactDirectory,
        "webkit.cpu.json",
      );
      const flamegraphPath = prepareArtifact(
        request.artifactDirectory,
        "webkit.flamegraph.speedscope.json",
      );
      const heapPath = prepareArtifact(
        request.artifactDirectory,
        "webkit.heap.json",
      );
      return await withPreparedTrialPage(
        browser,
        request,
        async (page) => {
          const session = webkitInspectorSession(browser);
          const batchSize = await selectBenchmarkBatchSize(
            page,
            request.operations,
            request.batchSize,
            request.batchTargetMs,
            request.batchMaxSize,
          );
          const trackingComplete = new Promise<WebKitProfile>(
            (resolve) => {
              session.once<WebKitProfile>(
                "ScriptProfiler.trackingComplete",
                resolve,
              );
            },
          );
          await session.send("ScriptProfiler.startTracking", {
            includeSamples: true,
          });
          const workload = await executeBenchmarkWorkload(
            page,
            request.operations,
            batchSize,
          );
          await session.send("ScriptProfiler.stopTracking");
          const profile = await trackingComplete;
          if ((profile.samples?.stackTraces?.length ?? 0) === 0) {
            throw new Error(
              "WebKit CPU profile did not contain samples",
            );
          }
          fs.writeFileSync(cpuPath, JSON.stringify(profile));
          webkitArtifactFormat.writeFlamegraph(
            profile,
            flamegraphPath,
            request.targetUrl,
          );
          await settleBenchmarkPage(page);
          await session.send("Heap.enable");
          const heap = await session.send<{
            snapshotData?: string;
          }>("Heap.snapshot");
          if (!heap.snapshotData) {
            throw new Error(
              "WebKit heap snapshot returned no data",
            );
          }
          fs.writeFileSync(heapPath, heap.snapshotData);
          return {
            browser: identity,
            workload,
            cpu_active_ms:
              webkitArtifactFormat.cpuActiveMilliseconds(
                profile,
                request.targetUrl,
              ) /
              batchSize,
            js_heap_live_bytes:
              webkitArtifactFormat.liveHeapBytes(
                heap.snapshotData,
              ),
            artifacts: [
              describeArtifact(
                request.artifactDirectory,
                "cpu_profile",
                cpuPath,
                "WebKit ScriptProfiler JSON",
              ),
              describeArtifact(
                request.artifactDirectory,
                "flamegraph",
                flamegraphPath,
                "Speedscope sampled profile",
              ),
              describeArtifact(
                request.artifactDirectory,
                "js_heap",
                heapPath,
                "WebKit Heap snapshot JSON",
              ),
            ],
          };
        },
      );
    },
    async close() {
      await browser.close({ reason: "bperf trial lane complete" });
    },
  };
}

export const webkitTrialAdapter = {
  openTrialLane: openWebkitTrialLane,
} satisfies TrialEngineAdapter;
