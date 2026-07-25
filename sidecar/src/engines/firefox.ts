import fs from "node:fs";

import { firefox, type BrowserContext } from "playwright";

import { describeArtifact, prepareArtifact } from "../artifacts.ts";
import type {
  BrowserEvidence,
  EngineCapture,
  RuntimeAnchorEvidence,
  TrialCapture,
  TrialEngineAdapter,
  TrialLane,
} from "../contract.ts";
import { browserInfo } from "../playwright-runtime.ts";
import { runProbeWorkload } from "../probe-workload.ts";
import { measureRuntimeAnchor } from "../runtime-anchor.ts";
import {
  FirefoxDebugSession,
  freePort,
} from "./firefox-rdp.ts";
import {
  executeBenchmarkWorkload,
  selectBenchmarkBatchSize,
  settleBenchmarkPage,
  withPreparedTrialPage,
} from "../trial-workload.ts";
import { firefoxArtifactFormat } from "./firefox-artifacts.ts";
import { FirefoxHeapSnapshotFiles } from "./firefox-heap-snapshot-files.ts";

async function launchFirefoxCapture() {
  const port = await freePort();
  const heapSnapshots = new FirefoxHeapSnapshotFiles();
  const browser = await firefox.launch({
    headless: true,
    args: ["--start-debugger-server", String(port)],
    firefoxUserPrefs: {
      "devtools.debugger.remote-enabled": true,
      "devtools.debugger.prompt-connection": false,
    },
  });
  return {
    browser,
    connectDebug: () =>
      FirefoxDebugSession.connect(port, heapSnapshots),
    async close(reason: string) {
      const failures: unknown[] = [];
      try {
        await browser.close({ reason });
      } catch (error) {
        failures.push(error);
      }
      try {
        await heapSnapshots.close();
      } catch (error) {
        failures.push(error);
      }
      if (failures.length === 1) {
        throw failures[0];
      }
      if (failures.length > 1) {
        throw new AggregateError(
          failures,
          "Firefox browser and heap snapshot cleanup both failed",
        );
      }
    },
  };
}

export async function captureFirefox(
  artifactDirectory: string,
): Promise<EngineCapture> {
  const cpuPath = prepareArtifact(artifactDirectory, "firefox.cpu.json");
  const heapPath = prepareArtifact(
    artifactDirectory,
    "firefox.heap.fxsnapshot",
  );
  const flamegraphPath = prepareArtifact(
    artifactDirectory,
    "firefox.flamegraph.speedscope.json",
  );
  const capture = await launchFirefoxCapture();
  const { browser } = capture;

  let identity: BrowserEvidence;
  let context: BrowserContext | undefined;
  let debug: FirefoxDebugSession | undefined;
  let profileSource: string;
  let anchor: RuntimeAnchorEvidence;
  try {
    identity = browserInfo(browser);
    context = await browser.newContext();
    const page = await context.newPage();
    debug = await capture.connectDebug();
    anchor = await measureRuntimeAnchor(page);
    await debug.startProfiler();
    await runProbeWorkload(page);
    profileSource = await debug.captureProfile();

    await debug.captureHeap(heapPath);
  } finally {
    try {
      await context?.close();
    } finally {
      try {
        debug?.close();
      } finally {
        await capture.close("bperf doctor complete");
      }
    }
  }

  fs.writeFileSync(cpuPath, profileSource);
  const profile = firefoxArtifactFormat.parseProfile(profileSource);
  firefoxArtifactFormat.writeFlamegraph(profile, flamegraphPath);
  return {
    browser: identity,
    anchor,
    artifacts: [
      describeArtifact(
        artifactDirectory,
        "cpu_profile",
        cpuPath,
        "Gecko Profiler JSON",
      ),
      describeArtifact(
        artifactDirectory,
        "js_heap",
        heapPath,
        "Firefox .fxsnapshot",
      ),
      describeArtifact(
        artifactDirectory,
        "flamegraph",
        flamegraphPath,
        "Speedscope sampled profiles",
      ),
    ],
  };
}

async function openFirefoxTrialLane(): Promise<
  TrialLane<TrialCapture>
> {
  const capture = await launchFirefoxCapture();
  const { browser } = capture;
  const identity = browserInfo(browser);
  return {
    async capture(request) {
      const cpuPath = prepareArtifact(
        request.artifactDirectory,
        "firefox.cpu.json",
      );
      const flamegraphPath = prepareArtifact(
        request.artifactDirectory,
        "firefox.flamegraph.speedscope.json",
      );
      const heapPath = prepareArtifact(
        request.artifactDirectory,
        "firefox.heap.fxsnapshot",
      );
      let debug: FirefoxDebugSession | undefined;
      try {
        return await withPreparedTrialPage(
          browser,
          request,
          async (page) => {
            const batchSize = await selectBenchmarkBatchSize(
              page,
              request.operations,
              request.batchSize,
              request.batchTargetMs,
              request.batchMaxSize,
            );
            debug = await capture.connectDebug();
            await debug.startProfiler();
            const workload = await executeBenchmarkWorkload(
              page,
              request.operations,
              batchSize,
            );
            const profileSource = await debug.captureProfile();
            await settleBenchmarkPage(page);
            fs.writeFileSync(cpuPath, profileSource);
            const profile =
              firefoxArtifactFormat.parseProfile(profileSource);
            firefoxArtifactFormat.writeFlamegraph(
              profile,
              flamegraphPath,
              request.targetUrl,
            );
            const jsHeapLiveBytes =
              await debug.captureHeap(heapPath);
            return {
              browser: identity,
              workload,
              cpu_active_ms:
                firefoxArtifactFormat.cpuActiveMilliseconds(
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
                  "Gecko Profiler JSON",
                ),
                describeArtifact(
                  request.artifactDirectory,
                  "flamegraph",
                  flamegraphPath,
                  "Speedscope sampled profiles",
                ),
                describeArtifact(
                  request.artifactDirectory,
                  "js_heap",
                  heapPath,
                  "Firefox .fxsnapshot",
                ),
              ],
            };
          },
        );
      } finally {
        debug?.close();
      }
    },
    async close() {
      await capture.close("bperf trial lane complete");
    },
  };
}

export const firefoxTrialAdapter = {
  openTrialLane: openFirefoxTrialLane,
} satisfies TrialEngineAdapter;
