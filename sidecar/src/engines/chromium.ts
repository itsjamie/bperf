import fs from "node:fs";

import { chromium, type CDPSession } from "playwright";

import { describeArtifact, prepareArtifact } from "../artifacts.ts";
import type {
  EngineCapture,
  TrialCapture,
  TrialEngineAdapter,
  TrialLane,
} from "../contract.ts";
import { browserInfo } from "../playwright-runtime.ts";
import { runProbeWorkload } from "../probe-workload.ts";
import { measureRuntimeAnchor } from "../runtime-anchor.ts";
import { SpeedscopeBuilder } from "../speedscope.ts";
import {
  executeBenchmarkWorkload,
  selectBenchmarkBatchSize,
  settleBenchmarkPage,
  withPreparedTrialPage,
} from "../trial-workload.ts";

interface ChromiumCallFrame {
  functionName: string;
  url: string;
  lineNumber: number;
  columnNumber: number;
}

interface ChromiumProfileNode {
  id: number;
  callFrame: ChromiumCallFrame;
  children?: number[];
}

interface ChromiumProfile {
  nodes: ChromiumProfileNode[];
  samples?: number[];
  timeDeltas?: number[];
}

interface ChromiumHeapSnapshot {
  snapshot?: {
    meta?: {
      node_fields?: string[];
    };
  };
  nodes?: number[];
}

function profileNodes(profile: ChromiumProfile): {
  nodes: Map<number, ChromiumProfileNode>;
  parents: Map<number, number>;
} {
  const nodes = new Map(profile.nodes.map((node) => [node.id, node]));
  const parents = new Map<number, number>();
  for (const node of profile.nodes) {
    for (const child of node.children ?? []) {
      parents.set(child, node.id);
    }
  }
  return { nodes, parents };
}

function targetNodePredicate(
  profile: ChromiumProfile,
  targetUrl: string,
): (nodeId: number) => boolean {
  const { nodes, parents } = profileNodes(profile);
  const cache = new Map<number, boolean>();
  function belongs(nodeId: number): boolean {
    const cached = cache.get(nodeId);
    if (cached !== undefined) return cached;
    const node = nodes.get(nodeId);
    const parent = parents.get(nodeId);
    const result =
      Boolean(node?.callFrame.url.startsWith(targetUrl)) ||
      (parent !== undefined && belongs(parent));
    cache.set(nodeId, result);
    return result;
  }
  return belongs;
}

function cpuActiveMilliseconds(
  profile: ChromiumProfile,
  targetUrl: string,
): number {
  const belongs = targetNodePredicate(profile, targetUrl);
  const duration = (profile.samples ?? []).reduce((sum, nodeId, index) => {
    const delta = profile.timeDeltas?.[index] ?? 0;
    return sum + (belongs(nodeId) && delta > 0 ? delta : 0);
  }, 0);
  if (!(duration > 0)) {
    throw new Error("Chromium CPU profile has no positive sample duration");
  }
  return duration / 1_000;
}

function liveHeapBytes(filePath: string): number {
  const snapshot = JSON.parse(
    fs.readFileSync(filePath, "utf8"),
  ) as ChromiumHeapSnapshot;
  const fields = snapshot.snapshot?.meta?.node_fields;
  const nodes = snapshot.nodes;
  const selfSize = fields?.indexOf("self_size") ?? -1;
  if (
    !fields ||
    selfSize < 0 ||
    !Array.isArray(nodes) ||
    nodes.length % fields.length !== 0
  ) {
    throw new Error("Chromium emitted an invalid V8 heap snapshot");
  }

  let total = 0;
  for (let index = selfSize; index < nodes.length; index += fields.length) {
    const size = nodes[index];
    if (!Number.isSafeInteger(size) || size < 0) {
      throw new Error("Chromium heap snapshot contains an invalid node size");
    }
    total += size;
  }
  if (!Number.isSafeInteger(total) || total <= 0) {
    throw new Error("Chromium heap snapshot contains no live heap bytes");
  }
  return total;
}

async function captureHeapSnapshot(
  session: CDPSession,
  heapPath: string,
): Promise<number> {
  await session.send("HeapProfiler.enable");
  await session.send("HeapProfiler.collectGarbage");
  const heapFile = fs.openSync(heapPath, "w");
  let chunkCount = 0;
  const onChunk = ({ chunk }: { chunk: string }) => {
    fs.writeSync(heapFile, chunk);
    chunkCount += 1;
  };
  session.on("HeapProfiler.addHeapSnapshotChunk", onChunk);
  try {
    await session.send("HeapProfiler.takeHeapSnapshot", {
      reportProgress: false,
      captureNumericValue: true,
    });
  } finally {
    session.off("HeapProfiler.addHeapSnapshotChunk", onChunk);
    fs.closeSync(heapFile);
  }
  if (chunkCount === 0) {
    throw new Error("Chromium heap snapshot emitted no chunks");
  }
  return liveHeapBytes(heapPath);
}

function writeFlamegraph(
  profile: ChromiumProfile,
  filePath: string,
  targetUrl?: string,
): void {
  const builder = new SpeedscopeBuilder("Chromium CPU");
  const { nodes, parents } = profileNodes(profile);

  const stackCache = new Map<number, number[]>();
  function stackFor(nodeId: number): number[] {
    const cached = stackCache.get(nodeId);
    if (cached) return cached;

    const node = nodes.get(nodeId);
    if (!node) return [];
    const parent = parents.get(nodeId);
    const stack = parent === undefined ? [] : [...stackFor(parent)];
    stack.push(
      builder.frame({
        name: node.callFrame.functionName,
        file: node.callFrame.url,
        line: node.callFrame.lineNumber,
        col: node.callFrame.columnNumber,
      }),
    );
    stackCache.set(nodeId, stack);
    return stack;
  }

  const belongs = targetUrl
    ? targetNodePredicate(profile, targetUrl)
    : () => true;
  const entries = (profile.samples ?? []).flatMap((nodeId, index) =>
    belongs(nodeId)
      ? [
          {
            sample: stackFor(nodeId),
            weight: Math.max(profile.timeDeltas?.[index] ?? 0, 1),
          },
        ]
      : [],
  );
  builder.sampledProfile({
    name: "Chromium renderer JavaScript",
    unit: "microseconds",
    samples: entries.map(({ sample }) => sample),
    weights: entries.map(({ weight }) => weight),
  });
  builder.write(filePath);
}

export const chromiumArtifactFormat = {
  cpuActiveMilliseconds,
  liveHeapBytes,
  writeFlamegraph,
} as const;

export async function captureChromium(
  artifactDirectory: string,
): Promise<EngineCapture> {
  const cpuPath = prepareArtifact(
    artifactDirectory,
    "chromium.cpu.cpuprofile",
  );
  const heapPath = prepareArtifact(
    artifactDirectory,
    "chromium.heap.heapsnapshot",
  );
  const flamegraphPath = prepareArtifact(
    artifactDirectory,
    "chromium.flamegraph.speedscope.json",
  );
  const browser = await chromium.launch({ headless: true });

  try {
    const identity = browserInfo(browser);
    const context = await browser.newContext();
    const page = await context.newPage();
    const session = await context.newCDPSession(page);
    const anchor = await measureRuntimeAnchor(page);

    await session.send("Profiler.enable");
    await session.send("Profiler.start");
    await runProbeWorkload(page);
    const { profile } = (await session.send(
      "Profiler.stop",
    )) as { profile: ChromiumProfile };
    await settleBenchmarkPage(page);
    if ((profile.samples?.length ?? 0) < 50) {
      throw new Error(
        "Chromium CPU profile did not contain enough samples",
      );
    }
    fs.writeFileSync(cpuPath, JSON.stringify(profile));
    chromiumArtifactFormat.writeFlamegraph(profile, flamegraphPath);

    await captureHeapSnapshot(session, heapPath);

    return {
      browser: identity,
      anchor,
      artifacts: [
        describeArtifact(
          artifactDirectory,
          "cpu_profile",
          cpuPath,
          "V8 CPU profile",
        ),
        describeArtifact(
          artifactDirectory,
          "js_heap",
          heapPath,
          "V8 heap snapshot",
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

async function openChromiumTrialLane(): Promise<
  TrialLane<TrialCapture>
> {
  const browser = await chromium.launch({
    headless: true,
  });
  const identity = browserInfo(browser);
  return {
    async capture(request) {
      const cpuPath = prepareArtifact(
        request.artifactDirectory,
        "chromium.cpu.cpuprofile",
      );
      const flamegraphPath = prepareArtifact(
        request.artifactDirectory,
        "chromium.flamegraph.speedscope.json",
      );
      const heapPath = prepareArtifact(
        request.artifactDirectory,
        "chromium.heap.heapsnapshot",
      );
      return await withPreparedTrialPage(
        browser,
        request,
        async (page) => {
          const session = await page.context().newCDPSession(page);
          const batchSize = await selectBenchmarkBatchSize(
            page,
            request.operations,
            request.batchSize,
            request.batchTargetMs,
            request.batchMaxSize,
          );
          await session.send("Profiler.enable");
          await session.send("Profiler.start");
          const workload = await executeBenchmarkWorkload(
            page,
            request.operations,
            batchSize,
          );
          const { profile } = (await session.send(
            "Profiler.stop",
          )) as { profile: ChromiumProfile };
          if ((profile.samples?.length ?? 0) === 0) {
            throw new Error(
              "Chromium CPU profile did not contain samples",
            );
          }
          fs.writeFileSync(cpuPath, JSON.stringify(profile));
          chromiumArtifactFormat.writeFlamegraph(
            profile,
            flamegraphPath,
            request.targetUrl,
          );
          await settleBenchmarkPage(page);
          const jsHeapLiveBytes = await captureHeapSnapshot(
            session,
            heapPath,
          );
          return {
            browser: identity,
            workload,
            cpu_active_ms:
              chromiumArtifactFormat.cpuActiveMilliseconds(
                profile,
                request.targetUrl,
              ) /
              batchSize,
            js_heap_live_bytes: jsHeapLiveBytes,
            artifacts: [
              describeArtifact(
                request.artifactDirectory,
                "cpu_profile",
                cpuPath,
                "V8 CPU profile",
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
                "V8 heap snapshot",
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

export const chromiumTrialAdapter = {
  openTrialLane: openChromiumTrialLane,
} satisfies TrialEngineAdapter;
